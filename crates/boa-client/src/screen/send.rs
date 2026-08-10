//! The sending side: an ffmpeg process, a pipe, and the packets that come out of it.
//!
//! Two pieces of parsing are all that stands between ffmpeg's stdout and the wire, and both are worth
//! being precise about because getting either wrong produces a stream that decodes to nothing.
//!
//! **Splitting the byte stream into NAL units.** Annex-B H.264 separates them with a three- or
//! four-byte start code (`00 00 01`, optionally preceded by another zero). A scanner that assumes one
//! length loses a byte from the front of every other unit.
//!
//! **Grouping NAL units into frames.** A picture can arrive as several NAL units: parameter sets
//! (SPS, PPS), an SEI message, then the slice itself. They have to be sent as one payload, because a
//! decoder handed a slice with no parameter sets in front of it has nothing to decode against. This
//! assumes **one slice per picture**, which is what both encoders used here produce by default; a
//! multi-slice stream would emit each slice as its own frame, which still decodes but wastes packets.

use std::io::Read as _;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context as _, Result};
use boa_proto::media::{fragment, write_fragment, MediaKind, PacketHeader};

use crate::media::Transport;
use crate::settings::ScreenSettings;

/// Something that can be shared.
///
/// A *choice*, not a set of dimensions. What somebody wants to say is "this screen" or "that window";
/// the resolution follows from the source and is capped only where a decoder would give up. The old
/// arrangement — a pixel slider and whichever display happened to be first — asked the wrong question
/// and then answered it without looking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// What ffmpeg is told to read.
    pub input: String,
    /// What the interface calls it.
    pub label: String,
    /// A window rather than a whole screen.
    pub window: bool,
}

/// Everything this machine can share, in the order to offer it.
///
/// Whole screens come first because they are what most shares are. Windows are listed where the
/// platform can capture one at all — which on macOS it cannot, not through ffmpeg: avfoundation
/// captures displays and nothing smaller. Single windows there need ScreenCaptureKit, which is the
/// same piece of work as capturing system audio without a loopback device, and is not done yet.
pub fn sources() -> Vec<Source> {
    #[cfg(target_os = "macos")]
    {
        let listing = super::ffmpeg::command()
            .and_then(|mut ffmpeg| {
                ffmpeg
                    .args(["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""])
                    .stderr(Stdio::piped())
                    .stdout(Stdio::null())
                    .output()
                    .ok()
            })
            .map(|out| String::from_utf8_lossy(&out.stderr).into_owned())
            .unwrap_or_default();
        let found = parse_screens(&listing);
        if found.is_empty() {
            // A machine whose device listing could not be read still gets an entry, because refusing
            // to offer anything is worse than offering the one that is almost always right.
            return vec![Source { input: "1".into(), label: "Screen".into(), window: false }];
        }
        found
    }
    #[cfg(target_os = "windows")]
    {
        let mut found =
            vec![Source { input: "desktop".into(), label: "Whole desktop".into(), window: false }];
        found.extend(windows_windows());
        found
    }
    #[cfg(target_os = "linux")]
    {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0.0".into());
        vec![Source { input: display.clone(), label: format!("Screen ({display})"), window: false }]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Pull the screens out of an avfoundation device listing.
///
/// The video half only, and only the entries that are displays: a webcam and a virtual camera sit in
/// the same list, and sharing one of those instead of the screen is a mistake that looks like a bug in
/// the receiver.
#[cfg(target_os = "macos")]
fn parse_screens(listing: &str) -> Vec<Source> {
    let mut found = Vec::new();
    let mut in_video = false;
    for line in listing.lines() {
        if line.contains("AVFoundation video devices") {
            in_video = true;
            continue;
        }
        if line.contains("AVFoundation audio devices") {
            in_video = false;
            continue;
        }
        if !in_video || !line.contains("Capture screen") {
            continue;
        }
        let Some(open) = line.rfind('[') else { continue };
        let Some(close) = line[open..].find(']').map(|offset| open + offset) else { continue };
        let index = line[open + 1..close].trim().to_string();
        let name = line[close + 1..].trim();
        found.push(Source {
            input: index,
            // "Capture screen 0" is ffmpeg's phrasing, not something to show somebody.
            label: match name.rsplit(' ').next().and_then(|n| n.parse::<u32>().ok()) {
                Some(0) => "Main screen".to_string(),
                Some(n) => format!("Screen {}", n + 1),
                None => name.to_string(),
            },
            window: false,
        });
    }
    found
}

/// The visible top-level windows, by title.
///
/// gdigrab addresses a window by its title, which is also its weakness: two windows with the same
/// title are indistinguishable, and a title that changes while sharing does not follow. It is what
/// Windows offers without a native capture path, and it works for the case people want — one
/// application, deliberately chosen.
#[cfg(target_os = "windows")]
fn windows_windows() -> Vec<Source> {
    let script = "Get-Process | Where-Object { $_.MainWindowTitle -ne '' } | \
                  ForEach-Object { $_.MainWindowTitle }";
    let Ok(out) = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .take(40)
        .map(|title| Source {
            input: format!("title={title}"),
            label: title.to_string(),
            window: true,
        })
        .collect()
}

/// A share in progress. Dropping it stops ffmpeg and the sending thread.
pub struct Share {
    child: Arc<std::sync::Mutex<Child>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub frames: Arc<AtomicU64>,
    pub packets: Arc<AtomicU64>,
    /// When capture began, so a share that produces nothing can be recognised as broken rather than
    /// slow. Four seconds of no frames is not a static screen — H.264 emits a keyframe every couple
    /// of seconds regardless — it is a capture that never started.
    pub started: Instant,
    /// The size ffmpeg was asked for, which is what the far side is told to expect.
    pub width: u32,
    pub height: u32,
    /// The machine's own sound, when it was asked for and a device could be found.
    audio: Option<super::DesktopAudio>,
    /// Why there is no sound, when there is none and there should have been. Shown to the user, since
    /// the fix is an install rather than a setting.
    pub audio_problem: Option<String>,
}

impl Share {
    /// Start capturing and sending.
    ///
    /// `settings` are the encoder's, and they are passed through: there is no ceiling here beyond
    /// what a decoder can be expected to handle (see [`super::MAX_DIMENSION`]). A machine with a fast
    /// encoder and a fast link can send 4K at 60 frames a second, and nothing in this project will
    /// ask it not to.
    pub fn start(
        transport: Transport,
        ssrc: u32,
        settings: &ScreenSettings,
        source: &Source,
        width: u32,
        height: u32,
    ) -> Result<Share> {
        // The audio goes first, because it is the part that can be *unavailable* rather than broken:
        // finding out before the picture starts means the user is told once, at the moment they
        // pressed the button, instead of wondering later.
        let (audio, audio_problem) = if settings.with_audio {
            match super::find_loopback() {
                Ok(loopback) => match transport
                    .try_clone()
                    .and_then(|transport| super::DesktopAudio::start(transport, ssrc, &loopback))
                {
                    Ok(audio) => (Some(audio), None),
                    Err(err) => (None, Some(format!("{err}"))),
                },
                Err(advice) => (None, Some(advice)),
            }
        } else {
            (None, None)
        };

        let Some(mut ffmpeg) = super::ffmpeg::command() else {
            bail!("{}", super::ffmpeg::advice());
        };

        let args = capture_args(settings, source, width, height);
        log::info!("screen: ffmpeg {}", args.join(" "));
        crate::diagnostics::note(&format!(
            "screen: sharing {} at up to {width}×{height}, {} fps",
            source.label, settings.fps
        ));

        let mut child = ffmpeg
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting ffmpeg")?;

        let stdout = child.stdout.take().ok_or_else(|| anyhow!("ffmpeg has no stdout"))?;
        let stderr = child.stderr.take();

        // ffmpeg's diagnostics go to the log rather than to a pipe nobody reads — which is also what
        // stops it blocking once the pipe's buffer fills, and it is the only place a "permission
        // denied" from the screen-recording prompt will appear.
        if let Some(stderr) = stderr {
            let builder = std::thread::Builder::new().name("boa-screen-log".into());
            let _ = builder.spawn(move || {
                use std::io::BufRead as _;
                for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        log::debug!("ffmpeg: {line}");
                    }
                }
            });
        }

        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let packets = Arc::new(AtomicU64::new(0));

        let thread = {
            let stop = stop.clone();
            let frames = frames.clone();
            let packets = packets.clone();
            std::thread::Builder::new()
                .name("boa-screen-tx".into())
                .spawn(move || {
                    if let Err(err) = pump(stdout, &transport, ssrc, &stop, &frames, &packets) {
                        log::error!("screen: {err:#}");
                        crate::diagnostics::note(&format!("screen: send stopped: {err:#}"));
                    }
                })
                .context("spawning the screen sender")?
        };

        Ok(Share {
            child: Arc::new(std::sync::Mutex::new(child)),
            stop,
            thread: Some(thread),
            frames,
            packets,
            started: Instant::now(),
            width,
            height,
            audio,
            audio_problem,
        })
    }
}

impl Share {
    /// The loopback device the sound is coming from, if any.
    pub fn audio_device(&self) -> Option<&str> {
        self.audio.as_ref().map(|audio| audio.device.as_str())
    }

    /// How many audio packets have gone out, for the diagnostics line.
    pub fn audio_packets(&self) -> u64 {
        self.audio.as_ref().map(|audio| audio.packets.load(Ordering::Relaxed)).unwrap_or(0)
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // The audio process first: it is the one holding a device somebody else may want back.
        self.audio = None;
        // Killed rather than asked politely. ffmpeg reading from a capture device does not notice a
        // closed stdout until it tries to write, which on an idle screen can be a whole frame
        // interval — and on macOS the screen-recording indicator stays lit until the process is gone.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        crate::diagnostics::note("screen: share ended");
    }
}

/// The ffmpeg command line for this platform.
///
/// Separated out and pure so the test below can check the flags that matter without running anything.
fn capture_args(
    settings: &ScreenSettings,
    source: &Source,
    width: u32,
    height: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        // No stdin, and no interactive keyboard handling: this is a pipe, and ffmpeg reading from a
        // terminal it does not have produces a stream of warnings.
        "-nostdin".into(),
    ];

    args.extend(platform_input(settings.fps, source));

    args.extend([
        // Scale to fit, keeping the aspect. `-2` rather than `-1` on the derived edge, because H.264
        // requires even dimensions and an odd one is rejected outright.
        // `decrease` and nothing else: the picture keeps the source's own resolution unless it is
        // larger than a decoder will take, in which case it is fitted inside that. There is no
        // resolution to choose, because "share this screen" already says what to send.
        "-vf".into(),
        format!("scale={width}:{height}:force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p"),
        "-c:v".into(),
        encoder().into(),
    ]);

    // The encoders take different flags for the same two ideas — do not buffer, and hold this
    // bitrate — so each one gets its own.
    match encoder() {
        "h264_videotoolbox" => args.extend([
            "-realtime".into(),
            "1".into(),
            "-b:v".into(),
            format!("{}k", settings.kbps),
        ]),
        _ => args.extend([
            "-preset".into(),
            "veryfast".into(),
            // Latency over compression: no lookahead, no B-frames, no frame reordering. A B-frame
            // cannot be decoded until the picture *after* it arrives, which for a live screen means
            // showing everything one frame late for a few percent of bitrate.
            "-tune".into(),
            "zerolatency".into(),
            "-b:v".into(),
            format!("{}k", settings.kbps),
            "-maxrate".into(),
            format!("{}k", settings.kbps),
            "-bufsize".into(),
            format!("{}k", settings.kbps / 2),
        ]),
    }

    args.extend([
        // A keyframe every two seconds. That is the worst case for somebody who joins mid-share, or
        // who lost a packet and has to wait for a clean reference — and every keyframe is many times
        // the size of a delta, so more often is expensive on the very link this is trying to fit.
        "-g".into(),
        (settings.fps * 2).max(2).to_string(),
        // A high profile is fine — every decoder in use supports it — and CABAC is a free 10%.
        "-profile:v".into(),
        "high".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-an".into(),
        "-f".into(),
        "h264".into(),
        "-".into(),
    ]);
    args
}

/// The capture input for this platform and this source.
fn platform_input(fps: u32, source: &Source) -> Vec<String> {
    if cfg!(target_os = "macos") {
        vec![
            "-f".into(),
            "avfoundation".into(),
            "-capture_cursor".into(),
            "1".into(),
            "-framerate".into(),
            fps.to_string(),
            // No `-pix_fmt` on the *input*: avfoundation offers uyvy422 or bgr0 depending on the
            // machine, and naming one that a given display does not support fails the capture
            // outright. The filter chain converts whatever arrives.
            "-i".into(),
            source.input.clone(),
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            "-f".into(),
            "gdigrab".into(),
            "-framerate".into(),
            fps.to_string(),
            "-i".into(),
            source.input.clone(),
        ]
    } else {
        // X11. Wayland needs PipeWire and a portal dialogue, which is a different mechanism
        // altogether — under a Wayland session this fails and the log says why.
        vec![
            "-f".into(),
            "x11grab".into(),
            "-framerate".into(),
            fps.to_string(),
            "-i".into(),
            source.input.clone(),
        ]
    }
}

/// The best available H.264 encoder.
fn encoder() -> &'static str {
    // VideoToolbox on macOS: a hardware encoder, which for a 4K screen at 60 frames a second is the
    // difference between a spare core and every core. Elsewhere libx264, which is everywhere.
    if cfg!(target_os = "macos") {
        "h264_videotoolbox"
    } else {
        "libx264"
    }
}

/// Read ffmpeg's output and put it on the wire.
fn pump(
    mut stdout: std::process::ChildStdout,
    transport: &Transport,
    ssrc: u32,
    stop: &AtomicBool,
    frames: &AtomicU64,
    packets: &AtomicU64,
) -> Result<()> {
    let mut reader = Annexb::default();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut payload = Vec::with_capacity(1_200);
    let mut scratch = Vec::with_capacity(1_200);
    let mut seq: u32 = 0;
    let start = Instant::now();

    while !stop.load(Ordering::Acquire) {
        let read = stdout.read(&mut buffer).context("reading from ffmpeg")?;
        if read == 0 {
            log::info!("screen: ffmpeg finished");
            return Ok(());
        }

        for frame in reader.feed(&buffer[..read]) {
            frames.fetch_add(1, Ordering::Relaxed);
            let kind = if frame.keyframe { MediaKind::VideoKey } else { MediaKind::VideoDelta };
            // Milliseconds since the share started, the same for every fragment of one picture —
            // which is also how the far side knows which fragments belong together.
            let timestamp = start.elapsed().as_millis() as u32;

            for (index, count, chunk) in fragment(&frame.data) {
                write_fragment(index, count, chunk, &mut payload);
                seq = seq.wrapping_add(1);
                let header = PacketHeader { kind, ssrc, seq, timestamp };
                match transport.send(header, &payload, &mut scratch) {
                    Ok(()) => {
                        packets.fetch_add(1, Ordering::Relaxed);
                    }
                    // One packet failing is not a reason to stop sharing: the link may be briefly
                    // full, and the next keyframe repairs whatever this one broke.
                    Err(err) => log::debug!("screen: sending: {err:#}"),
                }
            }
        }
    }
    Ok(())
}

/// One picture, ready to send.
pub struct Picture {
    pub data: Vec<u8>,
    /// Whether it can be decoded on its own — an IDR, with its parameter sets.
    pub keyframe: bool,
}

/// Splits an Annex-B byte stream into pictures.
///
/// Two things here were wrong in the obvious version, and both were found by the test that feeds the
/// same stream one byte at a time.
///
/// **The stream does not restart at each read.** A pipe delivers whatever it has, never frame-aligned,
/// so a NAL unit is routinely split across reads. Rescanning only for start codes loses the unit whose
/// own start code was consumed by the previous call — `in_nal` is the memory of "what is left in the
/// buffer is the middle of a unit".
///
/// **A unit does not extend to the next unit's payload.** The bytes between them are the start code,
/// three or four of them, and including those in the previous unit corrupts it.
#[derive(Default)]
pub struct Annexb {
    /// Bytes not yet formed into a complete NAL unit.
    pending: Vec<u8>,
    /// Whether `pending` begins inside a unit rather than at a start code.
    in_nal: bool,
    /// Parameter sets and SEI seen since the last picture, to be sent in front of the next slice.
    prefix: Vec<u8>,
    /// Whether the prefix contains an SPS or PPS, which makes the next picture independently
    /// decodable.
    prefix_has_parameters: bool,
}

impl Annexb {
    /// Feed some bytes; get back whatever pictures they completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Picture> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();

        // Where each unit begins. The first entry is zero when the buffer already holds the middle of
        // one; the rest are the payload offsets of the start codes found now.
        let mut boundaries: Vec<usize> = Vec::new();
        if self.in_nal {
            boundaries.push(0);
        }
        boundaries.extend(start_codes(&self.pending));
        if boundaries.len() < 2 {
            // Only one boundary means the last unit is still arriving; nothing can be completed yet.
            return out;
        }

        for pair in boundaries.windows(2) {
            let (from, next) = (pair[0], pair[1]);
            let nal = &self.pending[from..nal_end(&self.pending, next)];

            let Some(kind) = nal_type(nal) else { continue };
            match kind {
                // A slice: this completes a picture.
                1 | 5 => {
                    let mut data = std::mem::take(&mut self.prefix);
                    append_nal(&mut data, nal);
                    let keyframe = kind == 5 || self.prefix_has_parameters;
                    self.prefix_has_parameters = false;
                    out.push(Picture { data, keyframe });
                }
                // Parameter sets and SEI belong in front of the next slice.
                6..=9 => {
                    if kind == 7 || kind == 8 {
                        self.prefix_has_parameters = true;
                    }
                    append_nal(&mut self.prefix, nal);
                }
                // Anything else is passed through with the next picture rather than dropped.
                _ => append_nal(&mut self.prefix, nal),
            }
        }

        // Everything up to the last boundary has been dealt with; what follows it is the unit still
        // arriving. `drain` rather than a fresh Vec: this runs sixty times a second.
        let consumed = *boundaries.last().expect("checked above");
        self.pending.drain(..consumed);
        self.in_nal = true;
        out
    }
}

/// Write one NAL unit into a picture, with the start code a decoder needs.
///
/// Four bytes rather than three. Both are legal; the four-byte form is what every encoder emits for
/// the first unit of a picture, and using one length everywhere means the packetiser's output looks
/// the same whatever it was parsed from.
fn append_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// Where the unit before `next_start` ends.
///
/// `next_start` is the offset of the *payload* after a start code, so the three bytes before it are
/// `00 00 01` and there may be a fourth zero in front of those. Any further trailing zeros are
/// padding the standard allows and decoders ignore, so they come off too — which also makes the
/// result independent of which start-code length the encoder used.
fn nal_end(data: &[u8], next_start: usize) -> usize {
    let mut end = next_start.saturating_sub(3);
    while end > 0 && data[end - 1] == 0 {
        end -= 1;
    }
    end
}

/// Payload offsets of every Annex-B start code in `data`.
///
/// A start code is `00 00 01`, and it may be preceded by an extra zero (`00 00 00 01`). The offset
/// returned is of the *payload*, so a caller never has to know which length it was — the matching
/// [`nal_end`] takes both off the previous unit.
fn start_codes(data: &[u8]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            found.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    found
}

/// The NAL unit type of a unit whose start code has already been stripped.
fn nal_type(nal: &[u8]) -> Option<u8> {
    // The trailing zeroes of the *next* start code are part of this slice as it was cut, so an
    // apparently empty unit is possible and is not a type.
    let first = nal.iter().position(|byte| *byte != 0)?;
    Some(nal[first] & 0x1F)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an Annex-B stream from (type, payload) pairs, alternating start-code lengths so both
    /// are exercised.
    fn stream(units: &[(u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, (kind, payload)) in units.iter().enumerate() {
            if index % 2 == 0 {
                out.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                out.extend_from_slice(&[0, 0, 1]);
            }
            out.push(*kind);
            out.extend_from_slice(payload);
        }
        // A trailing start code, standing in for the next unit that has not arrived yet.
        out.extend_from_slice(&[0, 0, 1, 1]);
        out
    }

    #[test]
    fn both_start_code_lengths_are_found() {
        let data = [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 1, 0x41, 0xBB];
        // Offsets are of the payload, not of the start code, so a caller never has to know.
        assert_eq!(start_codes(&data), vec![4, 9]);
        assert!(start_codes(&[0, 0, 0]).is_empty());
        assert!(start_codes(&[]).is_empty());
    }

    #[test]
    fn a_units_type_is_its_low_five_bits() {
        assert_eq!(nal_type(&[0x65]), Some(5), "an IDR slice");
        assert_eq!(nal_type(&[0x41]), Some(1), "a non-IDR slice");
        assert_eq!(nal_type(&[0x67]), Some(7), "an SPS");
        assert_eq!(nal_type(&[0x68]), Some(8), "a PPS");
        assert_eq!(nal_type(&[]), None);
        assert_eq!(nal_type(&[0, 0]), None, "trailing zeroes are not a unit");
    }

    /// The grouping rule: parameter sets ride with the slice that follows them, so a decoder never
    /// receives a picture it has nothing to decode against.
    #[test]
    fn parameter_sets_are_sent_with_the_next_picture() {
        let mut reader = Annexb::default();
        let data = stream(&[
            (0x67, b"sps"),
            (0x68, b"pps"),
            (0x65, b"idr slice"),
            (0x41, b"delta slice"),
        ]);
        let pictures = reader.feed(&data);

        assert_eq!(pictures.len(), 2);
        assert!(pictures[0].keyframe);
        // The first picture carries the parameter sets and the slice, each behind a start code.
        assert!(pictures[0].data.windows(3).any(|w| w == b"sps"));
        assert!(pictures[0].data.windows(3).any(|w| w == b"pps"));
        assert!(pictures[0].data.windows(9).any(|w| w == b"idr slice"));
        assert_eq!(&pictures[0].data[..4], &[0, 0, 0, 1], "a picture starts with a start code");
        // Three units in, three start codes out.
        assert_eq!(
            pictures[0].data.windows(4).filter(|w| *w == [0, 0, 0, 1]).count(),
            3,
            "each unit keeps its own start code"
        );
        // And no start code was left *inside* a unit's data.
        assert!(!pictures[0].data.ends_with(&[0, 0, 1]), "the next unit's code must not be here");

        assert!(!pictures[1].keyframe);
        assert!(!pictures[1].data.windows(3).any(|w| w == b"sps"), "not repeated");
    }

    /// The stream arrives in whatever chunks the pipe felt like, which is never frame-aligned.
    #[test]
    fn a_stream_split_at_every_byte_gives_the_same_pictures() {
        let data = stream(&[(0x67, b"sps"), (0x65, b"idr"), (0x41, b"one"), (0x41, b"two")]);

        let mut whole = Annexb::default();
        let expected: Vec<bool> = whole.feed(&data).iter().map(|p| p.keyframe).collect();
        assert_eq!(expected, vec![true, false, false]);

        let mut byte_by_byte = Annexb::default();
        let mut got = Vec::new();
        for byte in &data {
            got.extend(byte_by_byte.feed(&[*byte]).into_iter().map(|p| p.keyframe));
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn a_units_end_is_before_the_next_start_code_whichever_length_it_was() {
        // `payload 00 00 01 next` — a three-byte code, so the unit ends at 3.
        let three = [0xAA, 0xBB, 0xCC, 0, 0, 1, 0x41];
        assert_eq!(nal_end(&three, 6), 3);
        // `payload 00 00 00 01 next` — four bytes, and the unit still ends at 3.
        let four = [0xAA, 0xBB, 0xCC, 0, 0, 0, 1, 0x41];
        assert_eq!(nal_end(&four, 7), 3);
        // Trailing padding zeros come off as well, which is what makes the two agree.
        let padded = [0xAA, 0, 0, 0, 0, 0, 1, 0x41];
        assert_eq!(nal_end(&padded, 6), 1);
    }

    #[test]
    fn an_incomplete_picture_is_not_emitted_early() {
        let mut reader = Annexb::default();
        // A start code and a slice header, with no following start code: the unit is not finished.
        assert!(reader.feed(&[0, 0, 0, 1, 0x65, 1, 2, 3]).is_empty());
        // Now the next one arrives, which ends the first.
        let pictures = reader.feed(&[0, 0, 1, 0x41]);
        assert_eq!(pictures.len(), 1);
        assert!(pictures[0].keyframe);
    }

    /// A slice with no parameter sets in front of it and no IDR is a delta, and must not be
    /// announced as a keyframe — a watcher that trusted it would decode garbage.
    #[test]
    fn a_delta_is_never_mistaken_for_a_keyframe() {
        let mut reader = Annexb::default();
        let pictures = reader.feed(&stream(&[(0x41, b"delta"), (0x41, b"delta")]));
        assert!(pictures.iter().all(|p| !p.keyframe));
    }

    /// The listing has cameras and virtual cameras in it, and the audio half repeats the numbering.
    /// Offering a webcam as "your screen" is a mistake that looks like a bug in the receiver.
    #[cfg(target_os = "macos")]
    #[test]
    fn only_the_screens_are_offered_and_they_are_named_for_people() {
        let listing = "\
[AVFoundation indev @ 0x7c7101c140] AVFoundation video devices:
[AVFoundation indev @ 0x7c7101c140] [0] Elgato HD60 X
[AVFoundation indev @ 0x7c7101c140] [1] OBS Virtual Camera
[AVFoundation indev @ 0x7c7101c140] [2] Capture screen 0
[AVFoundation indev @ 0x7c7101c140] [3] Capture screen 1
[AVFoundation indev @ 0x7c7101c140] AVFoundation audio devices:
[AVFoundation indev @ 0x7c7101c140] [2] Some Microphone";
        let screens = parse_screens(listing);
        assert_eq!(screens.len(), 2);
        assert_eq!(screens[0].input, "2");
        assert_eq!(screens[0].label, "Main screen");
        assert_eq!(screens[1].input, "3");
        assert_eq!(screens[1].label, "Screen 2");
        assert!(screens.iter().all(|s| !s.window));

        assert!(parse_screens("nothing here").is_empty());
        // The microphone in the audio half must not become a screen.
        assert!(parse_screens(
            "[x] AVFoundation audio devices:\n[x] [0] Capture screen 0"
        )
        .is_empty());
    }

    /// Whatever the machine says, there is always something to offer — refusing to offer anything is
    /// worse than offering the one that is nearly always right.
    #[test]
    fn there_is_always_at_least_one_source() {
        assert!(!sources().is_empty());
    }

    #[test]
    fn the_command_line_carries_the_settings_and_nothing_that_adds_delay() {
        let settings = ScreenSettings { max_dimension: 1920, fps: 60, kbps: 8_000, with_audio: false };
        let source = Source { input: "2".into(), label: "Main screen".into(), window: false };
        let args = capture_args(&settings, &source, 1920, 1080).join(" ");

        assert!(args.contains("scale=1920:1080"), "{args}");
        assert!(args.contains("-i 2"), "the chosen source, not a guess: {args}");
        assert!(args.contains("-b:v 8000k"), "{args}");
        // Two seconds between keyframes.
        assert!(args.contains("-g 120"), "{args}");
        // Annex-B on stdout, which is what the packetiser expects.
        assert!(args.ends_with("-f h264 -"), "{args}");
        // Even dimensions, or H.264 refuses the stream outright. `force_divisible_by` does it in the
        // one scale pass that also fits the source; the two-stage `trunc(iw/2)*2` version it replaced
        // scaled twice for the same result.
        assert!(args.contains("force_divisible_by=2"), "{args}");
        assert!(args.contains("-nostdin"), "{args}");
    }

    /// The picture keeps the source's resolution unless a decoder would refuse it — there is no
    /// resolution setting, because choosing a screen already says what to send.
    #[test]
    fn the_source_keeps_its_own_resolution_up_to_the_cap() {
        let settings =
            ScreenSettings { max_dimension: 3_840, fps: 60, kbps: 20_000, with_audio: false };
        let source = Source { input: "2".into(), label: "Main screen".into(), window: false };
        let args = capture_args(&settings, &source, 3_840, 2_160).join(" ");
        assert!(args.contains("force_original_aspect_ratio=decrease"), "{args}");
        // Even dimensions without a second scale pass, which the two-stage version needed.
        assert!(args.contains("force_divisible_by=2"), "{args}");
    }

    #[test]
    fn a_generous_bitrate_is_passed_through_rather_than_capped() {
        // The point of the project: 4K at 120 with a 100 Mbit/s target is somebody's LAN, and the
        // command line says so.
        let settings =
            ScreenSettings { max_dimension: 3_840, fps: 120, kbps: 100_000, with_audio: false };
        let source = Source { input: "desktop".into(), label: "Whole desktop".into(), window: false };
        let args = capture_args(&settings, &source, 3_840, 2_160).join(" ");
        assert!(args.contains("-b:v 100000k"), "{args}");
        assert!(args.contains("-g 240"), "{args}");
    }
}
