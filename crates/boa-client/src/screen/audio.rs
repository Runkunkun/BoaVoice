//! The sound of the machine you are sharing, sent along with the picture.
//!
//! This is harder than it sounds, and not for a technical reason: **no desktop operating system lets
//! a program record its own output without help.** Microphones have a permission model; system audio
//! mostly has nothing at all, because it was never a thing applications were expected to do. So each
//! platform needs a different arrangement, and two of the three need something installed:
//!
//! * **macOS** — a virtual output device that loops back to an input. [BlackHole] is the usual free
//!   one; Loopback and (historically) Soundflower do the same job. Once one exists, the machine's
//!   output is routed through it and ffmpeg reads it as an ordinary input.
//! * **Linux** — PulseAudio and PipeWire both expose a *monitor source* per output, which is exactly
//!   this and needs nothing installed. The best-supported platform for once.
//! * **Windows** — WASAPI can do loopback capture natively, but ffmpeg's Windows input does not
//!   expose it, so it needs a virtual device (`virtual-audio-capturer`, from the
//!   screen-capture-recorder package) or a cable.
//!
//! [`find_loopback`] looks for whatever is there and, when there is nothing, returns advice rather
//! than an error — because "your share has no sound" with no explanation is the worst outcome, and
//! the fix is a five-minute install.
//!
//! The audio is a **second ffmpeg process**, not a second stream in the video one. Multiplexing both
//! into a container and demuxing it in Rust would add a container format and a demuxer to the
//! critical path, to synchronise two things that are separately timestamped on the wire anyway. Two
//! processes, one reading a display and one reading a device, is less machinery and fails
//! independently: a machine with no loopback device still shares its screen.
//!
//! [BlackHole]: https://existential.audio/blackhole/

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use boa_proto::media::{MediaKind, PacketHeader, VOICE_FRAME_SAMPLES, VOICE_SAMPLE_RATE};

use crate::media::Transport;

/// Opus bitrate for a shared machine's audio, in bits per second.
///
/// 96 kbit/s stereo. Much higher than voice's 32, and for a good reason: this carries music, game
/// audio and video soundtracks — full-band material where Opus at 32 is audibly lossy, unlike speech.
/// It is still small next to the picture it accompanies, which is measured in megabits.
const DESKTOP_BITRATE: i32 = 96_000;

/// Samples in one packet: 20 ms of interleaved stereo.
const FRAME_SAMPLES: usize = VOICE_FRAME_SAMPLES * 2;

/// A device that can be recorded to capture the machine's own output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loopback {
    /// ffmpeg's `-f` value.
    pub format: &'static str,
    /// ffmpeg's `-i` value.
    pub input: String,
    /// What to call it in the interface.
    pub label: String,
}

/// Look for a loopback device, or explain what to install.
pub fn find_loopback() -> Result<Loopback, String> {
    #[cfg(target_os = "macos")]
    {
        match macos_loopback() {
            Some(loopback) => Ok(loopback),
            None => Err("no loopback device found. Install BlackHole \
                 (brew install blackhole-2ch), then set it as the output — or build a Multi-Output \
                 Device in Audio MIDI Setup so you can hear it too."
                .to_string()),
        }
    }
    #[cfg(target_os = "linux")]
    {
        Ok(pulse_monitor())
    }
    #[cfg(target_os = "windows")]
    {
        // Not probed: ffmpeg's dshow device listing is slow and this name is the one the
        // screen-capture-recorder package installs. If it is absent the process fails at once and
        // the log says so, which is a better trade than a two-second delay on every share.
        Ok(Loopback {
            format: "dshow",
            input: "audio=virtual-audio-capturer".to_string(),
            label: "virtual-audio-capturer".to_string(),
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err("sharing desktop audio is not supported on this platform".to_string())
    }
}

/// Names that mean "this is a loopback", in the order they are preferred.
///
/// Matched case-insensitively against a device's name. The order matters only when a machine has
/// several: BlackHole is the one most people install, and a 2-channel one is what a stereo share
/// wants.
const LOOPBACK_NAMES: [&str; 6] =
    ["blackhole 2ch", "blackhole", "loopback audio", "soundflower (2ch)", "soundflower", "vb-cable"];

/// Find a loopback among avfoundation's audio devices.
#[cfg(target_os = "macos")]
fn macos_loopback() -> Option<Loopback> {
    let listing = device_listing()?;
    let (index, name) = parse_audio_devices(&listing)
        .into_iter()
        .find_map(|(index, name)| {
            let lowered = name.to_lowercase();
            LOOPBACK_NAMES.iter().any(|candidate| lowered.contains(candidate)).then_some((index, name))
        })?;
    Some(Loopback {
        format: "avfoundation",
        // `:index` — the colon separates video from audio, and an empty video part means audio only.
        input: format!(":{index}"),
        label: name,
    })
}

#[cfg(target_os = "macos")]
fn device_listing() -> Option<String> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .ok()?;
    // The listing goes to stderr, and the command "fails" — listing devices is not a conversion.
    Some(String::from_utf8_lossy(&output.stderr).into_owned())
}

/// Pull `[3] Auna Mic CM900` out of the *audio* half of an avfoundation listing.
///
/// The two halves are numbered independently, so a parser that ignores the headings returns a video
/// index for an audio device — which produces a share whose "audio" is a webcam.
pub fn parse_audio_devices(listing: &str) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    let mut in_audio = false;
    for line in listing.lines() {
        if line.contains("AVFoundation video devices") {
            in_audio = false;
            continue;
        }
        if line.contains("AVFoundation audio devices") {
            in_audio = true;
            continue;
        }
        if !in_audio {
            continue;
        }
        // Two bracketed fields per line — the log's own tag, then the index — so the last one before
        // the name is the one wanted.
        let Some(open) = line.rfind('[') else { continue };
        let Some(close) = line[open..].find(']').map(|offset| open + offset) else { continue };
        let Ok(index) = line[open + 1..close].trim().parse::<u32>() else { continue };
        let name = line[close + 1..].trim().to_string();
        if !name.is_empty() {
            found.push((index, name));
        }
    }
    found
}

/// The PulseAudio or PipeWire monitor source for the default output.
#[cfg(target_os = "linux")]
fn pulse_monitor() -> Loopback {
    // `pactl` names the monitor exactly; `@DEFAULT_MONITOR@` is the fallback, which PulseAudio
    // resolves itself and PipeWire's compatibility layer also understands.
    let named = Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!sink.is_empty()).then(|| format!("{sink}.monitor"))
        });
    let input = named.unwrap_or_else(|| "@DEFAULT_MONITOR@".to_string());
    Loopback { format: "pulse", label: input.clone(), input }
}

/// A running desktop-audio stream. Dropping it stops ffmpeg and the sending thread.
pub struct DesktopAudio {
    child: Arc<std::sync::Mutex<Child>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub packets: Arc<AtomicU64>,
    pub device: String,
}

impl DesktopAudio {
    pub fn start(transport: Transport, ssrc: u32, loopback: &Loopback) -> Result<DesktopAudio> {
        let args = capture_args(loopback);
        log::info!("screen: audio from {} — ffmpeg {}", loopback.label, args.join(" "));
        crate::diagnostics::note(&format!("screen: desktop audio from {}", loopback.label));

        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .context("starting ffmpeg for desktop audio")?;

        let stdout = child.stdout.take().ok_or_else(|| anyhow!("ffmpeg has no stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            let builder = std::thread::Builder::new().name("boa-desktop-audio-log".into());
            let _ = builder.spawn(move || {
                use std::io::BufRead as _;
                for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        log::debug!("ffmpeg/audio: {line}");
                    }
                }
            });
        }

        let stop = Arc::new(AtomicBool::new(false));
        let packets = Arc::new(AtomicU64::new(0));
        let thread = {
            let stop = stop.clone();
            let packets = packets.clone();
            std::thread::Builder::new()
                .name("boa-desktop-audio".into())
                .spawn(move || {
                    if let Err(err) = pump(stdout, &transport, ssrc, &stop, &packets) {
                        log::error!("screen: desktop audio stopped: {err:#}");
                        crate::diagnostics::note(&format!("screen: desktop audio stopped: {err:#}"));
                    }
                })
                .context("spawning the desktop audio sender")?
        };

        Ok(DesktopAudio {
            child: Arc::new(std::sync::Mutex::new(child)),
            stop,
            thread: Some(thread),
            packets,
            device: loopback.label.clone(),
        })
    }
}

impl Drop for DesktopAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// ffmpeg's arguments: read the device, give me raw stereo floats at 48 kHz.
///
/// Raw PCM rather than asking ffmpeg to encode Opus. It is 384 kB/s through a pipe, which is nothing,
/// and it means the Opus encoder is the same one the voice path already uses at the same settings —
/// one codec configuration to reason about instead of two, and no container to parse.
fn capture_args(loopback: &Loopback) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-nostdin".into(),
        "-f".into(),
        loopback.format.into(),
        "-i".into(),
        loopback.input.clone(),
        // Resampled and downmixed by ffmpeg rather than here: a loopback device may run at 44.1 kHz
        // or have more than two channels, and ffmpeg's resampler is better than anything worth
        // writing for this.
        "-ac".into(),
        "2".into(),
        "-ar".into(),
        VOICE_SAMPLE_RATE.to_string(),
        "-f".into(),
        "f32le".into(),
        "-".into(),
    ]
}

/// Read PCM, encode, send.
fn pump(
    mut stdout: std::process::ChildStdout,
    transport: &Transport,
    ssrc: u32,
    stop: &AtomicBool,
    packets: &AtomicU64,
) -> Result<()> {
    let mut encoder = opus::Encoder::new(
        VOICE_SAMPLE_RATE,
        opus::Channels::Stereo,
        // `Audio`, not `Voip`. The opposite of the voice path, and deliberately: this is music and
        // game audio, where the codec should preserve the full band rather than optimise for
        // intelligibility.
        opus::Application::Audio,
    )
    .map_err(|err| anyhow!("starting the desktop audio encoder: {err}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(DESKTOP_BITRATE))
        .map_err(|err| anyhow!("setting the bitrate: {err}"))?;

    // Bytes from the pipe, which arrive in whatever size the pipe felt like — never a whole number
    // of frames, hence the leftover buffer.
    let mut raw = vec![0u8; 16 * 1024];
    let mut leftover: Vec<u8> = Vec::with_capacity(4 * FRAME_SAMPLES + 4);
    let mut pcm = vec![0.0f32; FRAME_SAMPLES];
    let mut encoded = vec![0u8; 4_000];
    let mut scratch = Vec::with_capacity(1_200);
    let mut seq: u32 = 0;
    let mut timestamp: u32 = 0;

    while !stop.load(Ordering::Acquire) {
        let read = stdout.read(&mut raw).context("reading desktop audio from ffmpeg")?;
        if read == 0 {
            log::info!("screen: desktop audio finished");
            return Ok(());
        }
        leftover.extend_from_slice(&raw[..read]);

        // Four bytes per sample, two samples per frame.
        let bytes_per_packet = FRAME_SAMPLES * 4;
        while leftover.len() >= bytes_per_packet {
            for (slot, chunk) in
                pcm.iter_mut().zip(leftover[..bytes_per_packet].chunks_exact(4))
            {
                *slot = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            }
            leftover.drain(..bytes_per_packet);

            let length = match encoder.encode_float(&pcm, &mut encoded) {
                Ok(length) => length,
                Err(err) => {
                    log::warn!("screen: encoding desktop audio: {err}");
                    continue;
                }
            };
            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(VOICE_FRAME_SAMPLES as u32);
            let header =
                PacketHeader { kind: MediaKind::DesktopAudio, ssrc, seq, timestamp };
            match transport.send(header, &encoded[..length], &mut scratch) {
                Ok(()) => {
                    packets.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => log::debug!("screen: sending desktop audio: {err:#}"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "\
[AVFoundation indev @ 0x7c7101c140] AVFoundation video devices:
[AVFoundation indev @ 0x7c7101c140] [0] Elgato HD60 X
[AVFoundation indev @ 0x7c7101c140] [1] OBS Virtual Camera
[AVFoundation indev @ 0x7c7101c140] [2] Capture screen 0
[AVFoundation indev @ 0x7c7101c140] AVFoundation audio devices:
[AVFoundation indev @ 0x7c7101c140] [0] Elgato HD60 X
[AVFoundation indev @ 0x7c7101c140] [1] BlackHole 2ch
[AVFoundation indev @ 0x7c7101c140] [2] Nothing Headphone (a)
[AVFoundation indev @ 0x7c7101c140] [3] Auna Mic CM900";

    /// The two halves are numbered independently. A parser that ignores the headings hands back a
    /// video index for an audio device, and the share's "sound" is a webcam.
    #[test]
    fn only_the_audio_half_of_the_listing_is_read() {
        let devices = parse_audio_devices(LISTING);
        assert_eq!(devices.len(), 4);
        assert_eq!(devices[0], (0, "Elgato HD60 X".to_string()));
        assert_eq!(devices[1], (1, "BlackHole 2ch".to_string()));
        assert_eq!(devices[3], (3, "Auna Mic CM900".to_string()));
        // "Capture screen 0" is in the video half and must not appear.
        assert!(devices.iter().all(|(_, name)| !name.contains("Capture screen")));
    }

    #[test]
    fn nothing_is_found_in_an_empty_or_broken_listing() {
        assert!(parse_audio_devices("").is_empty());
        assert!(parse_audio_devices("no devices at all").is_empty());
        // Video only: a machine with no audio inputs.
        assert!(parse_audio_devices(
            "[x] AVFoundation video devices:\n[x] [0] Camera"
        )
        .is_empty());
    }

    /// The name matching, exercised through the same table the real lookup uses.
    #[test]
    fn a_loopback_is_recognised_by_name_and_a_microphone_is_not() {
        let pick = |listing: &str| {
            parse_audio_devices(listing).into_iter().find_map(|(index, name)| {
                let lowered = name.to_lowercase();
                LOOPBACK_NAMES
                    .iter()
                    .any(|candidate| lowered.contains(candidate))
                    .then_some((index, name))
            })
        };
        assert_eq!(pick(LISTING), Some((1, "BlackHole 2ch".to_string())));

        // A machine with only real inputs: nothing is picked, and the caller explains what to
        // install rather than sharing a microphone as if it were the desktop.
        let microphones = "\
[x] AVFoundation audio devices:
[x] [0] MacBook Pro Microphone
[x] [1] Auna Mic CM900";
        assert_eq!(pick(microphones), None);

        // Case and spelling variants people actually have.
        for name in ["BLACKHOLE 16ch", "Loopback Audio 2", "Soundflower (2ch)", "VB-Cable"] {
            let listing = format!("[x] AVFoundation audio devices:\n[x] [4] {name}");
            assert!(pick(&listing).is_some(), "{name} should be recognised");
        }
    }

    #[test]
    fn the_command_line_asks_for_raw_stereo_at_the_codecs_rate() {
        let loopback = Loopback {
            format: "avfoundation",
            input: ":1".into(),
            label: "BlackHole 2ch".into(),
        };
        let args = capture_args(&loopback).join(" ");
        assert!(args.contains("-f avfoundation -i :1"), "{args}");
        assert!(args.contains("-ac 2"), "stereo: {args}");
        assert!(args.contains("-ar 48000"), "the codec's rate: {args}");
        // Raw floats, so the Opus encoder here is the same one the voice path uses.
        assert!(args.ends_with("-f f32le -"), "{args}");
    }

    /// 20 ms of stereo is 960 frames, 1920 samples, 7680 bytes. Every one of those numbers appears in
    /// the pump loop, and getting one wrong desynchronises the stream permanently.
    #[test]
    fn a_packet_is_twenty_milliseconds_of_stereo() {
        assert_eq!(FRAME_SAMPLES, 1_920);
        assert_eq!(FRAME_SAMPLES * 4, 7_680);
        assert_eq!(VOICE_FRAME_SAMPLES, 960);
    }

    /// Opus at these settings, round-tripped, so a packet is known to fit one datagram.
    #[test]
    fn a_desktop_audio_frame_fits_a_datagram() {
        let mut encoder =
            opus::Encoder::new(VOICE_SAMPLE_RATE, opus::Channels::Stereo, opus::Application::Audio)
                .unwrap();
        encoder.set_bitrate(opus::Bitrate::Bits(DESKTOP_BITRATE)).unwrap();

        // Something broadband, which is the worst case for the encoder's output size — a quiet or
        // tonal signal would compress far better and hide a packet that is too big.
        let input: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| {
                let t = i as f32 / 96_000.0;
                0.5 * ((t * 12_000.0).sin() + (t * 3_000.0).sin()) / 2.0
            })
            .collect();

        let mut packet = vec![0u8; 4_000];
        let length = encoder.encode_float(&input, &mut packet).unwrap();
        assert!(length > 0);
        assert!(
            length + boa_proto::media::HEADER_LEN + boa_proto::media::TAG_LEN
                <= boa_proto::media::MAX_DATAGRAM,
            "a desktop audio packet must fit one datagram: {length} bytes"
        );

        let mut decoder =
            opus::Decoder::new(VOICE_SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut out = vec![0.0f32; FRAME_SAMPLES];
        // The return value is frames *per channel*, so half the interleaved length.
        assert_eq!(decoder.decode_float(&packet[..length], &mut out, false).unwrap(), 960);
    }
}
