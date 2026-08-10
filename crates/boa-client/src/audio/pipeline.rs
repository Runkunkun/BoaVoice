//! The voice session: two device callbacks, two threads, and the state they share.
//!
//! ```text
//!  input callback ──ring──▶ boa-voice-tx ──▶ gain, denoise, gate ──▶ Opus ──▶ seal ──▶ UDP
//!                                                                                       │
//!  output callback ◀──ring per speaker──── boa-voice-rx ◀── Opus ◀── open ◀── UDP ◀──────┘
//! ```
//!
//! Four threads and no locks between them. That is the design constraint, not an optimisation: the
//! two callbacks are scheduled in real time with deadlines of a few milliseconds, and anything that
//! could make one of them wait — a mutex the interface holds, an allocation, a syscall — is an
//! audible click in a conversation rather than a dropped frame in a picture. So:
//!
//! * The **callbacks** only convert sample formats and move samples through a [`Ring`].
//! * The **tx thread** does everything expensive on the way out: noise suppression, the gate, Opus,
//!   the AEAD seal, and the `send_to`.
//! * The **rx thread** does everything expensive on the way in: the AEAD open, Opus, and loss
//!   concealment.
//! * The **interface** reads a handful of atomics once per frame and never touches anything else.
//!
//! The one shared structure with any complexity is the speaker table, and it is a fixed array of
//! slots rather than a map for exactly this reason: the playback callback walks it every few
//! milliseconds and must never wait for the receive thread to finish inserting somebody.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use boa_proto::media::{MediaKind, PacketHeader, MAX_DATAGRAM, VOICE_FRAME_SAMPLES, VOICE_SAMPLE_RATE};
use boa_proto::{Id, SessionKey};
use cpal::traits::{DeviceTrait as _, StreamTrait as _};

use super::denoise::{amplitude_from_db, Cleanup, FRAME};
use super::resample::{to_mono, Resampler};
use super::ring::Ring;
use crate::media::{Transport, KEEPALIVE};

/// How many people can be heard at once.
///
/// Sixteen. Not a limit on channel size — it is a limit on *simultaneous speakers*, and a
/// conversation with sixteen people talking at once has already failed. A fixed array is what keeps
/// the playback callback lock-free, and the seventeenth speaker is dropped with a log line rather
/// than making the array a map.
pub const MAX_SPEAKERS: usize = 16;

/// Opus bitrate for voice, in bits per second.
///
/// 32 kbit/s mono. Opus at this rate is transparent for speech — the difference from 64 is not
/// audible on a voice — and it means eight people in a call cost about 300 kbit/s of downlink on a
/// self-hosted box's uplink. There is no reason to be stingier and little to gain by being more
/// generous, so it is a constant rather than a setting nobody would know how to set.
const VOICE_BITRATE: i32 = 32_000;

/// How long without a keepalive reply before the media path is called broken.
const MEDIA_TIMEOUT: Duration = Duration::from_secs(5);

/// Samples the capture ring holds: about half a second.
///
/// Generous on purpose. The tx thread can be descheduled for a while on a busy machine, and the cost
/// of a large ring is a few kilobytes, while the cost of a small one is dropped audio at exactly the
/// moment the machine is under load.
const CAPTURE_RING: usize = VOICE_SAMPLE_RATE as usize / 2;

/// Samples each speaker's ring holds: about a second.
///
/// Twice the sample rate, because the rings are interleaved stereo — a second of audio is two
/// seconds' worth of samples.
const PLAYBACK_RING: usize = VOICE_SAMPLE_RATE as usize * 2;

/// Where screen-share packets go when somebody is watching one.
///
/// A named type because the nesting is four deep and it appears in two signatures; bounded because
/// video that has backed up is video worth dropping.
type VideoTap = std::sync::mpsc::SyncSender<(PacketHeader, Vec<u8>)>;

/// One person we can hear.
struct Speaker {
    /// Their voice stream id, or 0 when the slot is free.
    ssrc: AtomicU32,
    /// Their user id, so the interface can set a volume for them.
    user: AtomicU64,
    /// Per-person volume as `f32` bits, 0…2.
    volume: AtomicU32,
    /// Decoded samples waiting to be played. Written by the rx thread, read by the callback.
    ring: Ring,
    /// Whether enough has arrived to start playing. See [`Shared::prime_samples`].
    primed: AtomicBool,
}

impl Speaker {
    fn free() -> Speaker {
        Speaker {
            ssrc: AtomicU32::new(0),
            user: AtomicU64::new(0),
            volume: AtomicU32::new(1.0f32.to_bits()),
            ring: Ring::new(PLAYBACK_RING),
            primed: AtomicBool::new(false),
        }
    }
}

/// Everything the four threads share. Atomics only.
pub struct Shared {
    // --- what the interface sets ---
    muted: AtomicBool,
    deafened: AtomicBool,
    push_to_talk: AtomicBool,
    talk_key_held: AtomicBool,
    suppress: AtomicBool,
    gain: AtomicU32,
    output_volume: AtomicU32,
    threshold_db: AtomicU32,
    hang_ms: AtomicU32,
    /// How many samples a speaker's ring must hold before playback starts.
    prime_samples: AtomicUsize,

    // --- what the interface reads ---
    input_level: AtomicU32,
    gate_open: AtomicBool,
    speaking: AtomicBool,
    media_ok: AtomicBool,
    packets_out: AtomicU64,
    packets_in: AtomicU64,
    concealed: AtomicU64,
    underruns: AtomicU64,

    // --- the plumbing ---
    stop: AtomicBool,
    capture: Ring,
    speakers: Vec<Speaker>,
    /// Which person each stream id belongs to, as the control plane reported it.
    ///
    /// Consulted by the receive thread when it claims a slot, so a stream's owner is known from its
    /// first packet rather than from whenever the interface next sets a volume.
    owners: Mutex<std::collections::HashMap<u32, Id>>,
    /// Where to put screen-share packets, when somebody is watching one.
    ///
    /// A mutex, unlike everything else here, and that is fine: it is touched by the receive thread
    /// (which already does AEAD and Opus per packet, so a lock is noise) and by the interface when a
    /// watch starts or stops. It is *not* touched by either audio callback, which is where the
    /// no-locks rule applies.
    video: Mutex<Option<VideoTap>>,
}

impl Shared {
    fn new(settings: &crate::settings::VoiceSettings) -> Shared {
        Shared {
            muted: AtomicBool::new(settings.muted),
            deafened: AtomicBool::new(settings.deafened),
            push_to_talk: AtomicBool::new(settings.push_to_talk),
            talk_key_held: AtomicBool::new(false),
            suppress: AtomicBool::new(settings.noise_suppression),
            gain: AtomicU32::new(settings.input_gain.to_bits()),
            output_volume: AtomicU32::new(settings.output_volume.to_bits()),
            threshold_db: AtomicU32::new(settings.gate_threshold_db.to_bits()),
            hang_ms: AtomicU32::new(settings.gate_hang_ms),
            prime_samples: AtomicUsize::new(prime_samples(settings.jitter_ms)),
            input_level: AtomicU32::new(0),
            gate_open: AtomicBool::new(false),
            speaking: AtomicBool::new(false),
            media_ok: AtomicBool::new(false),
            packets_out: AtomicU64::new(0),
            packets_in: AtomicU64::new(0),
            concealed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            capture: Ring::new(CAPTURE_RING),
            speakers: (0..MAX_SPEAKERS).map(|_| Speaker::free()).collect(),
            owners: Mutex::new(std::collections::HashMap::new()),
            video: Mutex::new(None),
        }
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    fn output_volume(&self) -> f32 {
        f32::from_bits(self.output_volume.load(Ordering::Relaxed))
    }

    fn threshold_db(&self) -> f32 {
        f32::from_bits(self.threshold_db.load(Ordering::Relaxed))
    }

    /// Whether audio should be captured and sent at all.
    fn transmitting(&self) -> bool {
        !self.muted.load(Ordering::Relaxed) && !self.deafened.load(Ordering::Relaxed)
    }

    /// The slot for an ssrc, claiming a free one if this is somebody new.
    ///
    /// Called only from the receive thread, which is what makes the claim safe without a lock: there
    /// is exactly one thread that ever writes an `ssrc`.
    fn slot_for(&self, ssrc: u32) -> Option<&Speaker> {
        if let Some(existing) =
            self.speakers.iter().find(|s| s.ssrc.load(Ordering::Acquire) == ssrc)
        {
            return Some(existing);
        }
        let free = self.speakers.iter().find(|s| s.ssrc.load(Ordering::Acquire) == 0)?;
        free.ring.clear();
        free.primed.store(false, Ordering::Release);
        // Whoever the control plane said this stream belongs to, if it has said yet.
        let user = self
            .owners
            .lock()
            .ok()
            .and_then(|owners| owners.get(&ssrc).copied())
            .unwrap_or(Id::NONE);
        free.user.store(user.0, Ordering::Relaxed);
        // Last, and with release ordering: the playback callback treats a non-zero ssrc as "this
        // slot is ready to read", so everything else has to be in place first.
        free.ssrc.store(ssrc, Ordering::Release);
        Some(free)
    }

    fn drop_speaker(&self, ssrc: u32) {
        for speaker in &self.speakers {
            if speaker.ssrc.load(Ordering::Acquire) == ssrc {
                speaker.ssrc.store(0, Ordering::Release);
                speaker.ring.clear();
            }
        }
    }
}

/// Samples of audio to hold before starting playback.
///
/// Doubled for interleaved stereo: the setting is in milliseconds, and a millisecond of stereo is two
/// samples per 48 kHz tick. Getting this wrong halves the jitter buffer everybody asked for.
fn prime_samples(jitter_ms: u32) -> usize {
    (VOICE_SAMPLE_RATE as usize * jitter_ms.clamp(20, 500) as usize / 1000) * 2
}

/// What the interface needs to draw, once per frame.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub input_level: f32,
    pub gate_open: bool,
    pub speaking: bool,
    pub threshold: f32,
    pub media_ok: bool,
    pub packets_out: u64,
    pub packets_in: u64,
    pub concealed: u64,
}

/// A live voice session. Dropping it ends the call.
pub struct VoiceSession {
    shared: Arc<Shared>,
    /// A spare handle on the media socket, for a screen share to send through. The same socket, which
    /// is what keeps the relay's one address binding valid for both streams.
    transport: Transport,
    /// The device streams. Kept alive because dropping a `cpal::Stream` stops it — and not `Send`
    /// on every platform, which is why they stay on the thread that built them (the interface's).
    _input: Option<cpal::Stream>,
    _output: Option<cpal::Stream>,
    tx: Option<std::thread::JoinHandle<()>>,
    rx: Option<std::thread::JoinHandle<()>>,
    /// Which channel this session belongs to, so a stale `VoiceReady` can be recognised.
    pub channel: Id,
    pub ssrc: u32,
    /// The ssrc our screen share would use — the control plane allocates it at join time.
    pub screen_ssrc: u32,
}

impl VoiceSession {
    /// Join: open the devices, start the threads, register with the relay.
    ///
    /// A failure to open the *input* is not fatal and does not abort the call: somebody with no
    /// microphone, or who has refused the permission, should still be able to listen. A failure to
    /// open the output is, because a voice call you cannot hear is not a voice call.
    pub fn start(
        relay: std::net::SocketAddr,
        key: SessionKey,
        ssrc: u32,
        channel: Id,
        settings: &crate::settings::VoiceSettings,
    ) -> Result<VoiceSession> {
        let shared = Arc::new(Shared::new(settings));
        let transport = Transport::open(relay, key)?;

        let input = match start_capture(&shared, settings) {
            Ok(stream) => Some(stream),
            Err(err) => {
                log::error!("audio: no microphone ({err:#}); joining as a listener");
                crate::diagnostics::note(&format!("audio: input unavailable: {err:#}"));
                None
            }
        };
        let output = start_playback(&shared, settings)
            .context("opening the output device")
            .inspect_err(|err| crate::diagnostics::note(&format!("audio: output failed: {err:#}")))?;

        let tx = spawn_tx(shared.clone(), transport.try_clone()?, ssrc);
        let spare = transport.try_clone()?;
        let rx = spawn_rx(shared.clone(), transport, ssrc);

        crate::diagnostics::note(&format!("voice: session on channel {channel}, ssrc {ssrc}"));
        Ok(VoiceSession {
            shared,
            transport: spare,
            _input: input,
            _output: Some(output),
            tx: Some(tx),
            rx: Some(rx),
            channel,
            ssrc,
            // The screen stream is the one after the voice stream — the server allocates them in
            // that order — but it is *told* to us in `ScreenStart`, so this is only a default.
            screen_ssrc: ssrc.wrapping_add(1),
        })
    }

    /// A handle on the media socket for a screen share.
    pub fn transport(&self) -> Result<Transport> {
        self.transport.try_clone()
    }

    /// Start decoding somebody's screen, replacing any decoder already running.
    ///
    /// The channel is bounded and its overflow is dropped: see [`Shared::video`].
    pub fn watch(&self, ssrc: u32) -> crate::screen::Watcher {
        // 120 packets is about two 4K keyframes' worth of fragments — enough that a decoder which
        // stalls for a frame catches up, small enough that one which stalls for a second does not
        // accumulate a backlog it would then play late.
        let (tx, rx) = std::sync::mpsc::sync_channel(120);
        if let Ok(mut video) = self.shared.video.lock() {
            *video = Some(tx);
        }
        crate::screen::Watcher::start(rx, ssrc)
    }

    /// Stop feeding any decoder.
    pub fn stop_watching(&self) {
        if let Ok(mut video) = self.shared.video.lock() {
            *video = None;
        }
    }

    pub fn status(&self) -> Status {
        Status {
            input_level: f32::from_bits(self.shared.input_level.load(Ordering::Relaxed)),
            gate_open: self.shared.gate_open.load(Ordering::Relaxed),
            speaking: self.shared.speaking.load(Ordering::Relaxed),
            threshold: amplitude_from_db(self.shared.threshold_db()),
            media_ok: self.shared.media_ok.load(Ordering::Relaxed),
            packets_out: self.shared.packets_out.load(Ordering::Relaxed),
            packets_in: self.shared.packets_in.load(Ordering::Relaxed),
            concealed: self.shared.concealed.load(Ordering::Relaxed),
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.shared.deafened.store(deafened, Ordering::Relaxed);
    }

    /// Push-to-talk: whether the key is down right now. Called every frame by the interface.
    pub fn set_talk_key(&self, held: bool) {
        self.shared.talk_key_held.store(held, Ordering::Relaxed);
    }

    /// Apply changed settings to a running call.
    ///
    /// Everything except the *devices*: swapping a microphone mid-call means tearing down a stream
    /// and building another, which the caller does by ending the session and starting a new one.
    pub fn apply(&self, settings: &crate::settings::VoiceSettings) {
        self.shared.suppress.store(settings.noise_suppression, Ordering::Relaxed);
        self.shared.push_to_talk.store(settings.push_to_talk, Ordering::Relaxed);
        self.shared.gain.store(settings.input_gain.to_bits(), Ordering::Relaxed);
        self.shared.output_volume.store(settings.output_volume.to_bits(), Ordering::Relaxed);
        self.shared.threshold_db.store(settings.gate_threshold_db.to_bits(), Ordering::Relaxed);
        self.shared.hang_ms.store(settings.gate_hang_ms, Ordering::Relaxed);
        self.shared
            .prime_samples
            .store(prime_samples(settings.jitter_ms), Ordering::Relaxed);
    }

    /// Say which person a stream id belongs to.
    ///
    /// The packets do not carry it — the control plane is what maps a stream to a person — so without
    /// this the mixer's slots have no owner and per-person volume matches nothing. Called for every
    /// voice state and every screen share the interface learns about, including the *screen* stream,
    /// which is how a share's desktop audio inherits its sharer's volume.
    pub fn attribute(&self, ssrc: u32, user: Id) {
        for speaker in &self.shared.speakers {
            if speaker.ssrc.load(Ordering::Acquire) == ssrc {
                speaker.user.store(user.0, Ordering::Relaxed);
            }
        }
        // Remembered even when no slot exists yet: a share announced before its first packet arrives
        // would otherwise never be attributed.
        if let Ok(mut pending) = self.shared.owners.lock() {
            pending.insert(ssrc, user);
        }
    }

    /// Set one person's volume, 0…2.
    pub fn set_user_volume(&self, user: Id, volume: f32) {
        for speaker in &self.shared.speakers {
            if speaker.user.load(Ordering::Relaxed) == user.0 {
                speaker.volume.store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
            }
        }
    }

    /// Somebody left: retire their slot so a new speaker can have it.
    pub fn forget(&self, ssrc: u32) {
        self.shared.drop_speaker(ssrc);
    }
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        // The streams go first: a callback that runs while the rings are being torn down would find
        // an empty ring, which is merely silence, but stopping the devices is also what releases the
        // microphone indicator on macOS — and leaving that on after a call has ended is alarming.
        self._input = None;
        self._output = None;
        // The threads wake within one read timeout and see the flag.
        for handle in [self.tx.take(), self.rx.take()].into_iter().flatten() {
            let _ = handle.join();
        }
        crate::diagnostics::note("voice: session ended");
    }
}

// --------------------------------------------------------------------------- //
// Capture
// --------------------------------------------------------------------------- //

/// Pick a stream config, preferring 48 kHz so no resampling is needed.
fn pick_config(supported: cpal::SupportedStreamConfig, wanted: u32) -> cpal::StreamConfig {
    let mut config = supported.config();
    if config.sample_rate != wanted {
        log::info!(
            "audio: device runs at {} Hz, converting to {wanted} Hz",
            config.sample_rate
        );
    }
    // A buffer size is *not* requested. The temptation is to ask for something small to cut latency,
    // and on several backends an unsupported request fails the whole stream rather than being
    // rounded — so the device's default is used and the jitter buffer absorbs the difference.
    config.buffer_size = cpal::BufferSize::Default;
    config
}

fn start_capture(
    shared: &Arc<Shared>,
    settings: &crate::settings::VoiceSettings,
) -> Result<cpal::Stream> {
    let device = super::devices::open_input(settings.input_device.as_deref())
        .ok_or_else(|| anyhow!("no input device"))?;
    let supported = device.default_input_config().context("asking for an input config")?;
    let format = supported.sample_format();
    let config = pick_config(supported, VOICE_SAMPLE_RATE);
    let channels = config.channels as usize;
    let rate = config.sample_rate;

    log::info!("audio: capturing from {device} at {rate} Hz, {channels} channel(s), {format:?}");
    crate::diagnostics::note(&format!("audio: input {device} {rate} Hz {channels}ch"));

    let shared = shared.clone();
    // Scratch buffers, moved into the callback so it never allocates. They grow once, on the first
    // callback, and are reused for the life of the stream.
    let mut mono: Vec<f32> = Vec::with_capacity(2_048);
    let mut converted: Vec<f32> = Vec::with_capacity(2_048);
    let mut resampler = Resampler::new(rate, VOICE_SAMPLE_RATE);

    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                mono.clear();
                converted.clear();
                to_mono(data, channels, &mut mono);
                resampler.process(&mono, &mut converted);
                // Dropped rather than blocked on: if the tx thread has fallen behind, the newest
                // audio is what matters. `push` returning short is the only "error" possible here.
                shared.capture.push(&converted);
            },
            capture_error,
            None,
        ),
        // I16 and U16 exist on plenty of hardware, and a session that refuses to start because a
        // headset reports integers would be a session most people cannot have.
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                mono.clear();
                converted.clear();
                for frame in data.chunks_exact(channels) {
                    let sum: f32 = frame.iter().map(|s| *s as f32 / 32_768.0).sum();
                    mono.push(sum / channels as f32);
                }
                resampler.process(&mono, &mut converted);
                shared.capture.push(&converted);
            },
            capture_error,
            None,
        ),
        other => return Err(anyhow!("this build cannot capture {other:?} samples")),
    }
    .context("building the input stream")?;

    stream.play().context("starting the input stream")?;
    Ok(stream)
}

fn capture_error(err: cpal::Error) {
    // Logged rather than propagated: by the time a stream error arrives there is nobody to return it
    // to, and the useful outcome is a line in the log next to the panic that did not happen.
    log::error!("audio: capture: {err}");
}

// --------------------------------------------------------------------------- //
// The outbound thread
// --------------------------------------------------------------------------- //

fn spawn_tx(shared: Arc<Shared>, transport: Transport, ssrc: u32) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("boa-voice-tx".into())
        .spawn(move || {
            if let Err(err) = run_tx(&shared, &transport, ssrc) {
                log::error!("audio: the send loop stopped: {err:#}");
                crate::diagnostics::note(&format!("voice: tx stopped: {err:#}"));
            }
        })
        .expect("spawning a thread")
}

fn run_tx(shared: &Shared, transport: &Transport, ssrc: u32) -> Result<()> {
    let mut encoder = opus::Encoder::new(
        VOICE_SAMPLE_RATE,
        opus::Channels::Mono,
        // `Voip` rather than `Audio`: it tells Opus this is speech, which turns on the parts of the
        // codec that matter here — better behaviour under loss, and a bias towards intelligibility
        // over fidelity when the bitrate is tight.
        opus::Application::Voip,
    )
    .map_err(|err| anyhow!("starting the encoder: {err}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(VOICE_BITRATE))
        .map_err(|err| anyhow!("setting the bitrate: {err}"))?;
    // In-band forward error correction: Opus adds a low-bitrate copy of the *previous* frame to each
    // packet, which the decoder can use when one goes missing. It costs a little bandwidth and is
    // exactly the trade wanted on a path where loss is the expected failure.
    let _ = encoder.set_inband_fec(true);
    let _ = encoder.set_packet_loss_perc(10);

    let mut cleanup = Cleanup::new(
        shared.threshold_db(),
        shared.hang_ms.load(Ordering::Relaxed),
        shared.suppress.load(Ordering::Relaxed),
    );

    let mut frame = [0.0f32; FRAME];
    let mut pending: Vec<f32> = Vec::with_capacity(VOICE_FRAME_SAMPLES * 2);
    let mut encoded = vec![0u8; 4_000];
    let mut scratch = Vec::with_capacity(MAX_DATAGRAM);
    let mut seq: u32 = 0;
    let mut timestamp: u32 = 0;
    let mut keepalive_seq: u32 = 0;
    let mut next_keepalive = Instant::now();
    let mut was_speaking = false;
    // Set while the gate is shut, so exactly one packet is sent to close a talk spurt rather than a
    // stream of them.
    let mut sent_since_gate_closed = false;

    while !shared.stop.load(Ordering::Acquire) {
        // Keepalives go out whether or not anybody is talking: they are what keeps the relay's
        // address binding and the NAT mapping alive, and what tells us the media path works.
        if Instant::now() >= next_keepalive {
            next_keepalive = Instant::now() + KEEPALIVE;
            keepalive_seq = keepalive_seq.wrapping_add(1);
            if let Err(err) = transport.register(ssrc, keepalive_seq, &mut scratch) {
                log::debug!("audio: keepalive: {err:#}");
            }
        }

        // Settings can change under us at any moment; picking them up here rather than per packet
        // keeps the hot path free of branches on atomics that almost never change.
        cleanup.set_suppression(shared.suppress.load(Ordering::Relaxed));
        cleanup.set_threshold(shared.threshold_db());
        cleanup.set_hang(shared.hang_ms.load(Ordering::Relaxed));

        if shared.capture.len() < FRAME {
            // Nothing to do. A short sleep rather than a spin: at 10 ms per frame, waking every
            // 2 ms is responsive and costs nothing measurable.
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }

        let taken = shared.capture.pop(&mut frame);
        if taken < FRAME {
            // A partial frame means the ring emptied between the check and the read; the rest is
            // silence, which is better than holding a half-frame across an iteration.
            frame[taken..].fill(0.0);
        }

        let forced = shared
            .push_to_talk
            .load(Ordering::Relaxed)
            .then(|| shared.talk_key_held.load(Ordering::Relaxed));
        let outcome = cleanup.process(&mut frame, shared.gain(), forced);

        shared.input_level.store(outcome.level.to_bits(), Ordering::Relaxed);
        shared.gate_open.store(outcome.open, Ordering::Relaxed);

        let transmitting = shared.transmitting();
        let speaking = transmitting && outcome.open;
        if speaking != was_speaking {
            was_speaking = speaking;
            shared.speaking.store(speaking, Ordering::Relaxed);
        }

        pending.extend_from_slice(&frame);
        while pending.len() >= VOICE_FRAME_SAMPLES {
            let chunk: Vec<f32> = pending.drain(..VOICE_FRAME_SAMPLES).collect();
            timestamp = timestamp.wrapping_add(VOICE_FRAME_SAMPLES as u32);

            if !transmitting {
                sent_since_gate_closed = false;
                continue;
            }
            if !outcome.open {
                // One trailing packet after the gate closes, then nothing. Without the trailing
                // packet the receiver's last frame is whatever was mid-word; with a stream of them
                // the gate would save no bandwidth at all.
                if sent_since_gate_closed {
                    continue;
                }
                sent_since_gate_closed = true;
            } else {
                sent_since_gate_closed = false;
            }

            let length = match encoder.encode_float(&chunk, &mut encoded) {
                Ok(length) => length,
                Err(err) => {
                    log::warn!("audio: encoding: {err}");
                    continue;
                }
            };
            seq = seq.wrapping_add(1);
            let header = PacketHeader { kind: MediaKind::Voice, ssrc, seq, timestamp };
            match transport.send(header, &encoded[..length], &mut scratch) {
                Ok(()) => {
                    shared.packets_out.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => log::debug!("audio: sending: {err:#}"),
            }
        }
    }

    // Leaving: tell the world we stopped talking, so nobody is left lit up in a roster.
    shared.speaking.store(false, Ordering::Relaxed);
    Ok(())
}

// --------------------------------------------------------------------------- //
// The inbound thread
// --------------------------------------------------------------------------- //

fn spawn_rx(shared: Arc<Shared>, transport: Transport, own_ssrc: u32) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("boa-voice-rx".into())
        .spawn(move || {
            if let Err(err) = run_rx(&shared, &transport, own_ssrc) {
                log::error!("audio: the receive loop stopped: {err:#}");
                crate::diagnostics::note(&format!("voice: rx stopped: {err:#}"));
            }
        })
        .expect("spawning a thread")
}

/// The most consecutive lost packets worth concealing.
///
/// Five, which is 100 ms. Beyond that a "gap" is not loss — it is a talk spurt that ended, and
/// concealing it would invent half a second of speech-shaped noise where there was silence.
const MAX_CONCEAL: i32 = 5;

/// What the receive loop should do with a packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SeqAction {
    /// Decode it, after concealing this many packets that never arrived.
    Play { conceal: u32 },
    /// A duplicate, or one that arrived after its own successor. Playing it would insert a fragment
    /// of the past into the present.
    Drop,
}

/// Decide from the sequence number alone.
fn classify(expected: Option<u32>, seq: u32) -> SeqAction {
    let Some(expected) = expected else {
        // The first packet from this sender. Whatever its number, it is where the stream starts.
        return SeqAction::Play { conceal: 0 };
    };
    // Signed distance, computed through the wrap so a stream that has been running for a day (and
    // passed 2^32 packets) is not read as a jump of four billion.
    let distance = seq.wrapping_sub(expected) as i32;
    match distance {
        0 => SeqAction::Play { conceal: 0 },
        d if d > 0 && d <= MAX_CONCEAL => SeqAction::Play { conceal: d as u32 },
        d if d > MAX_CONCEAL => SeqAction::Play { conceal: 0 },
        _ => SeqAction::Drop,
    }
}

/// One decoder per sender, with the sequence number needed to spot loss.
struct Incoming {
    ssrc: u32,
    decoder: opus::Decoder,
    /// The sequence number expected next, for detecting a gap.
    expect: Option<u32>,
    /// Whether this stream is stereo. Voice is not; a screen share's audio is.
    stereo: bool,
}

/// Push decoded audio into a slot's interleaved-stereo ring.
///
/// `frames` is Opus's own return value: samples *per channel*. A mono stream is widened by writing
/// each sample into both channels, which is done here rather than at the mixer so that everything
/// past this point can assume one layout.
fn push_stereo(ring: &Ring, pcm: &[f32], frames: usize, stereo: bool, widened: &mut Vec<f32>) {
    if stereo {
        ring.push(&pcm[..frames * 2]);
        return;
    }
    widened.clear();
    for sample in &pcm[..frames] {
        widened.push(*sample);
        widened.push(*sample);
    }
    ring.push(widened);
}

fn run_rx(shared: &Shared, transport: &Transport, own_ssrc: u32) -> Result<()> {
    let mut buffer = [0u8; MAX_DATAGRAM];
    let mut decoders: Vec<Incoming> = Vec::with_capacity(MAX_SPEAKERS);
    // Four frames' worth, so a 60 ms Opus packet (the longest anything here sends) fits with room to
    // spare, stereo included.
    let mut pcm = vec![0.0f32; VOICE_FRAME_SAMPLES * 8];
    let mut widened = Vec::with_capacity(VOICE_FRAME_SAMPLES * 2);
    let mut last_keepalive = Instant::now();

    while !shared.stop.load(Ordering::Acquire) {
        // A media path that has gone quiet is worth saying out loud: the UDP port is the one
        // firewalls block, and "chat works but nobody can hear anybody" is the commonest
        // self-hosting mistake there is.
        let alive = last_keepalive.elapsed() < MEDIA_TIMEOUT;
        shared.media_ok.store(alive, Ordering::Relaxed);

        let Some((header, payload)) = transport.recv(&mut buffer)? else { continue };

        if header.kind == MediaKind::Keepalive {
            // The relay echoes our own registration, which is the only positive evidence a client
            // can have that its packets are arriving.
            if header.ssrc == own_ssrc {
                last_keepalive = Instant::now();
            }
            continue;
        }
        if header.kind.is_video() {
            // Screen media arrives on the same socket. Handed to whichever decoder is watching, and
            // dropped when there is none — or when its queue is full, which is the right answer for
            // video: a decoder that has fallen behind wants the newest picture, not a backlog.
            if let Ok(video) = shared.video.lock() {
                if let Some(tx) = video.as_ref() {
                    let _ = tx.try_send((header, payload));
                }
            }
            continue;
        }
        let stereo = match header.kind {
            MediaKind::Voice => false,
            // A screen share's own audio: stereo, because there the channel separation is the
            // content rather than a way of placing somebody in a room.
            MediaKind::DesktopAudio => true,
            _ => continue,
        };
        // Our own voice, echoed by a relay that should not have: dropping it here rather than
        // trusting the server is what stops a loop of hearing yourself.
        if header.ssrc == own_ssrc {
            continue;
        }
        shared.packets_in.fetch_add(1, Ordering::Relaxed);

        if shared.deafened.load(Ordering::Relaxed) {
            continue;
        }

        let Some(slot) = shared.slot_for(header.ssrc) else {
            log::debug!("audio: no free speaker slot for ssrc {}", header.ssrc);
            continue;
        };

        let index = match decoders.iter().position(|d| d.ssrc == header.ssrc) {
            Some(index) => index,
            None => {
                let channels = if stereo { opus::Channels::Stereo } else { opus::Channels::Mono };
                let decoder = opus::Decoder::new(VOICE_SAMPLE_RATE, channels)
                    .map_err(|err| anyhow!("starting a decoder: {err}"))?;
                decoders.retain(|d| {
                    shared.speakers.iter().any(|s| s.ssrc.load(Ordering::Acquire) == d.ssrc)
                });
                decoders.push(Incoming { ssrc: header.ssrc, decoder, expect: None, stereo });
                decoders.len() - 1
            }
        };
        let incoming = &mut decoders[index];

        match classify(incoming.expect, header.seq) {
            SeqAction::Drop => continue,
            SeqAction::Play { conceal } => {
                for _ in 0..conceal {
                    // Exactly one frame's worth of buffer, and that is load-bearing: concealment
                    // fills whatever it is given rather than producing one frame, so handing it the
                    // whole scratch buffer would invent four frames of audio for every packet lost —
                    // adding delay while stretching the gap it exists to hide.
                    let room = VOICE_FRAME_SAMPLES * if incoming.stereo { 2 } else { 1 };
                    let frame = &mut pcm[..room];
                    if let Ok(frames) = incoming.decoder.decode_float(&[], frame, false) {
                        push_stereo(&slot.ring, &pcm, frames, incoming.stereo, &mut widened);
                        shared.concealed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        incoming.expect = Some(header.seq.wrapping_add(1));

        match incoming.decoder.decode_float(&payload, &mut pcm, false) {
            Ok(frames) => {
                push_stereo(&slot.ring, &pcm, frames, incoming.stereo, &mut widened);
                if !slot.primed.load(Ordering::Relaxed)
                    && slot.ring.len() >= shared.prime_samples.load(Ordering::Relaxed)
                {
                    slot.primed.store(true, Ordering::Release);
                }
            }
            Err(err) => log::debug!("audio: decoding from {}: {err}", header.ssrc),
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------- //
// Playback
// --------------------------------------------------------------------------- //

fn start_playback(
    shared: &Arc<Shared>,
    settings: &crate::settings::VoiceSettings,
) -> Result<cpal::Stream> {
    let device = super::devices::open_output(settings.output_device.as_deref())
        .ok_or_else(|| anyhow!("no output device"))?;
    let supported = device.default_output_config().context("asking for an output config")?;
    let format = supported.sample_format();
    let config = pick_config(supported, VOICE_SAMPLE_RATE);
    let channels = config.channels as usize;
    let rate = config.sample_rate;

    log::info!("audio: playing to {device} at {rate} Hz, {channels} channel(s), {format:?}");
    crate::diagnostics::note(&format!("audio: output {device} {rate} Hz {channels}ch"));

    let shared = shared.clone();
    let mut scratch = Scratch::new(rate);
    // Interleaved stereo samples produced but not yet written out, so a buffer boundary does not
    // truncate half a frame.
    let mut ready: std::collections::VecDeque<f32> = std::collections::VecDeque::with_capacity(8_192);
    let input_per_output = VOICE_SAMPLE_RATE as f64 / rate.max(1) as f64;

    let stream = match format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let frames = data.len() / channels.max(1);
                fill(&shared, frames, input_per_output, &mut scratch, &mut ready);
                for frame in data.chunks_mut(channels) {
                    // Missing samples are silence, not the previous buffer repeated: a repeat is a
                    // buzz, silence is a gap, and a gap is much easier to recognise as loss.
                    let left = ready.pop_front().unwrap_or(0.0);
                    let right = ready.pop_front().unwrap_or(0.0);
                    write_frame(left, right, frame);
                }
            },
            playback_error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                let frames = data.len() / channels.max(1);
                fill(&shared, frames, input_per_output, &mut scratch, &mut ready);
                let mut float = [0.0f32; 8];
                for frame in data.chunks_mut(channels) {
                    let left = ready.pop_front().unwrap_or(0.0);
                    let right = ready.pop_front().unwrap_or(0.0);
                    let used = frame.len().min(float.len());
                    write_frame(left, right, &mut float[..used]);
                    for (slot, sample) in frame.iter_mut().zip(&float[..used]) {
                        *slot = (sample.clamp(-1.0, 1.0) * 32_767.0) as i16;
                    }
                }
            },
            playback_error,
            None,
        ),
        other => return Err(anyhow!("this build cannot play {other:?} samples")),
    }
    .context("building the output stream")?;

    stream.play().context("starting the output stream")?;
    Ok(stream)
}

/// Write one stereo pair across however many channels the device has.
///
/// Mono devices get the average rather than the left channel: a laptop with one speaker should not
/// lose whatever a screen share happened to pan right. Beyond two channels the extras get the
/// average too — this is a chat application, not a surround mixer, and centring is better than
/// leaving rear speakers silent.
fn write_frame(left: f32, right: f32, frame: &mut [f32]) {
    match frame.len() {
        0 => {}
        1 => frame[0] = (left + right) * 0.5,
        _ => {
            frame[0] = left;
            frame[1] = right;
            let centre = (left + right) * 0.5;
            for slot in frame.iter_mut().skip(2) {
                *slot = centre;
            }
        }
    }
}

fn playback_error(err: cpal::Error) {
    log::error!("audio: playback: {err}");
}

/// The playback callback's buffers and resamplers, allocated once.
///
/// A struct rather than seven parameters, and it exists because the callback may not allocate: every
/// buffer in here is sized on the first call and reused for the life of the stream.
struct Scratch {
    /// Interleaved stereo, the mix of everybody.
    mixed: Vec<f32>,
    /// Interleaved stereo, one speaker at a time.
    per_speaker: Vec<f32>,
    /// Deinterleaved, because a resampler works on one channel at a time.
    left: Vec<f32>,
    right: Vec<f32>,
    out_left: Vec<f32>,
    out_right: Vec<f32>,
    /// One per channel, and they must stay in step: two resamplers fed the same number of samples
    /// produce the same number out, and their fractional positions advance identically.
    left_resampler: Resampler,
    right_resampler: Resampler,
}

impl Scratch {
    fn new(device_rate: u32) -> Scratch {
        Scratch {
            mixed: Vec::new(),
            per_speaker: Vec::new(),
            left: Vec::new(),
            right: Vec::new(),
            out_left: Vec::new(),
            out_right: Vec::new(),
            left_resampler: Resampler::new(VOICE_SAMPLE_RATE, device_rate),
            right_resampler: Resampler::new(VOICE_SAMPLE_RATE, device_rate),
        }
    }

    fn resize(&mut self, frames: usize) {
        let samples = frames * 2;
        if self.mixed.len() < samples {
            self.mixed.resize(samples, 0.0);
            self.per_speaker.resize(samples, 0.0);
            self.left.resize(frames, 0.0);
            self.right.resize(frames, 0.0);
        }
    }
}

/// Mix enough 48 kHz stereo frames for `frames` output frames, convert, and queue them.
///
/// Runs inside the playback callback, so: no allocation, no locks, and no branch that can wait. The
/// mixing itself is a sum over at most [`MAX_SPEAKERS`] slots, which is a few hundred multiply-adds
/// per millisecond.
///
/// Everything here is **interleaved stereo**, including voice — a mono speaker is written into both
/// channels by the receive thread. Carrying voice as mono all the way to the device and only widening
/// at the end would be slightly cheaper and would leave nowhere to put a screen share's stereo audio,
/// which is the one stream where the channel separation is the content.
fn fill(
    shared: &Shared,
    frames: usize,
    input_per_output: f64,
    scratch: &mut Scratch,
    ready: &mut std::collections::VecDeque<f32>,
) {
    if ready.len() >= frames * 2 {
        return;
    }
    // Frames of 48 kHz audio needed for this many output frames, plus slack so rounding never leaves
    // the queue one short — which would be a click at every buffer boundary, a hundred times a second.
    let needed = ((frames as f64 * input_per_output).ceil() as usize) + 8;
    scratch.resize(needed);

    let deafened = shared.deafened.load(Ordering::Relaxed);
    let master = if deafened { 0.0 } else { shared.output_volume() };
    let samples = needed * 2;
    scratch.mixed[..samples].fill(0.0);

    if !deafened {
        for speaker in &shared.speakers {
            if speaker.ssrc.load(Ordering::Acquire) == 0 {
                continue;
            }
            // Priming: a stream that has not buffered its jitter allowance yet is silent rather than
            // stuttering. Without this, playback starts on the first packet and every subsequent
            // network hiccup is a gap.
            if !speaker.primed.load(Ordering::Acquire) {
                continue;
            }
            let taken = speaker.ring.pop(&mut scratch.per_speaker[..samples]);
            if taken < samples {
                // Underrun: re-prime rather than limping along one packet behind, and count it so the
                // settings screen can suggest a larger buffer.
                speaker.primed.store(false, Ordering::Release);
                shared.underruns.fetch_add(1, Ordering::Relaxed);
                scratch.per_speaker[taken..samples].fill(0.0);
            }
            let volume = f32::from_bits(speaker.volume.load(Ordering::Relaxed));
            for (out, sample) in
                scratch.mixed[..samples].iter_mut().zip(scratch.per_speaker[..samples].iter())
            {
                *out += sample * volume;
            }
        }
    }

    for sample in scratch.mixed[..samples].iter_mut() {
        // Clamped, not normalised. A limiter that pulled the whole mix down when two people talk at
        // once would make everybody quieter for as long as they overlap, which is worse than the rare
        // clip — voices at 32 kbit/s sum well below full scale in practice.
        *sample = (*sample * master).clamp(-1.0, 1.0);
    }

    if scratch.left_resampler.passthrough() {
        // The common case: the device runs at 48 kHz and the mix goes straight out.
        ready.extend(scratch.mixed[..samples].iter().copied());
        return;
    }

    // Deinterleave, convert each channel, interleave again. Two resamplers rather than one on the
    // interleaved buffer, because interpolating between adjacent samples of an interleaved stream
    // would blend the left channel into the right.
    for (frame, pair) in scratch.mixed[..samples].chunks_exact(2).enumerate() {
        scratch.left[frame] = pair[0];
        scratch.right[frame] = pair[1];
    }
    scratch.out_left.clear();
    scratch.out_right.clear();
    scratch.left_resampler.process(&scratch.left[..needed], &mut scratch.out_left);
    scratch.right_resampler.process(&scratch.right[..needed], &mut scratch.out_right);
    for (left, right) in scratch.out_left.iter().zip(scratch.out_right.iter()) {
        ready.push_back(*left);
        ready.push_back(*right);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> crate::settings::VoiceSettings {
        crate::settings::VoiceSettings::default()
    }

    /// The bug this pins: the *expected* packet must play, not be dropped as a duplicate. The first
    /// version dropped every second frame and concealed it, which halved the real audio and showed up
    /// only as a `concealed` count that was half the received count.
    #[test]
    fn the_expected_packet_plays() {
        assert_eq!(classify(Some(5), 5), SeqAction::Play { conceal: 0 });
        // And a whole in-order run stays in order.
        let mut expected = None;
        for seq in 1..=20u32 {
            assert_eq!(classify(expected, seq), SeqAction::Play { conceal: 0 }, "seq {seq}");
            expected = Some(seq.wrapping_add(1));
        }
    }

    #[test]
    fn a_short_gap_is_concealed_and_a_long_one_is_not() {
        assert_eq!(classify(Some(5), 6), SeqAction::Play { conceal: 1 });
        assert_eq!(classify(Some(5), 10), SeqAction::Play { conceal: 5 });
        // Beyond the limit it is a talk spurt resuming, not loss.
        assert_eq!(classify(Some(5), 11), SeqAction::Play { conceal: 0 });
        assert_eq!(classify(Some(5), 5_000), SeqAction::Play { conceal: 0 });
    }

    #[test]
    fn a_duplicate_or_a_reordered_packet_is_dropped() {
        assert_eq!(classify(Some(5), 4), SeqAction::Drop, "the one before the expected: a duplicate");
        assert_eq!(classify(Some(5), 1), SeqAction::Drop, "long overtaken");
    }

    #[test]
    fn the_first_packet_from_a_sender_is_always_played() {
        assert_eq!(classify(None, 1), SeqAction::Play { conceal: 0 });
        // Including one from a sender that has been running for a while before we joined.
        assert_eq!(classify(None, 900_000), SeqAction::Play { conceal: 0 });
    }

    /// A session that has sent four billion packets wraps its counter, and the arithmetic has to
    /// follow it — a naive subtraction would read the wrap as a jump of four billion and stop
    /// concealing anything for the rest of the call.
    #[test]
    fn the_sequence_counter_may_wrap() {
        assert_eq!(classify(Some(u32::MAX), 0), SeqAction::Play { conceal: 1 });
        assert_eq!(classify(Some(u32::MAX), u32::MAX), SeqAction::Play { conceal: 0 });
        assert_eq!(classify(Some(0), u32::MAX), SeqAction::Drop, "that is the one before");
    }

    #[test]
    fn the_prime_depth_follows_the_jitter_setting() {
        // 60 ms at 48 kHz is 2880 frames — and 5760 interleaved stereo samples, which is what the
        // rings hold. Forgetting the factor of two here would halve everybody's jitter buffer.
        assert_eq!(prime_samples(60), 5_760);
        assert_eq!(prime_samples(20), 1_920);
        // And a nonsensical value is clamped rather than producing a buffer of nothing.
        assert_eq!(prime_samples(0), prime_samples(20));
        assert_eq!(prime_samples(10_000), prime_samples(500));
    }

    #[test]
    fn a_slot_is_claimed_once_and_found_again() {
        let shared = Shared::new(&settings());
        let first = shared.slot_for(7).unwrap() as *const Speaker;
        let again = shared.slot_for(7).unwrap() as *const Speaker;
        assert_eq!(first, again, "the same sender must not take two slots");

        let other = shared.slot_for(9).unwrap() as *const Speaker;
        assert_ne!(first, other);
    }

    #[test]
    fn slots_run_out_rather_than_growing() {
        let shared = Shared::new(&settings());
        for ssrc in 1..=MAX_SPEAKERS as u32 {
            assert!(shared.slot_for(ssrc).is_some(), "{ssrc}");
        }
        // Sixteen people talking at once is a failed conversation, not a case to allocate for.
        assert!(shared.slot_for(999).is_none());

        // Somebody leaving frees theirs.
        shared.drop_speaker(1);
        assert!(shared.slot_for(999).is_some());
    }

    #[test]
    fn a_reused_slot_starts_empty_and_unprimed() {
        let shared = Shared::new(&settings());
        let slot = shared.slot_for(1).unwrap();
        slot.ring.push(&[0.5; 100]);
        slot.primed.store(true, Ordering::Release);

        shared.drop_speaker(1);
        let reused = shared.slot_for(2).unwrap();
        assert!(reused.ring.is_empty(), "the last speaker's audio must not play as the next one's");
        assert!(!reused.primed.load(Ordering::Acquire));
    }

    #[test]
    fn muting_and_deafening_both_stop_transmitting() {
        let shared = Shared::new(&settings());
        assert!(shared.transmitting());
        shared.muted.store(true, Ordering::Relaxed);
        assert!(!shared.transmitting());
        shared.muted.store(false, Ordering::Relaxed);
        shared.deafened.store(true, Ordering::Relaxed);
        assert!(!shared.transmitting(), "sending into a call you cannot hear is pure waste");
    }

    /// The mixer, driven directly: two speakers at different volumes, summed, with the master
    /// applied — and a deafened listener getting silence regardless.
    #[test]
    fn the_mixer_sums_speakers_and_respects_every_volume() {
        let shared = Shared::new(&settings());
        // Prime two speakers with a constant, which makes the arithmetic checkable by hand.
        for (ssrc, value, volume) in [(1u32, 0.2f32, 1.0f32), (2, 0.1, 0.5)] {
            let slot = shared.slot_for(ssrc).unwrap();
            slot.ring.push(&vec![value; 8_000]);
            slot.volume.store(volume.to_bits(), Ordering::Relaxed);
            slot.primed.store(true, Ordering::Release);
        }

        let mut scratch = Scratch::new(VOICE_SAMPLE_RATE);
        let mut ready = std::collections::VecDeque::new();
        fill(&shared, 480, 1.0, &mut scratch, &mut ready);

        // Two samples per frame now.
        assert!(ready.len() >= 960);
        let sample = ready[10];
        // 0.2 * 1.0 + 0.1 * 0.5 = 0.25
        assert!((sample - 0.25).abs() < 1e-4, "{sample}");

        // Deafened: nothing comes out, whatever is buffered.
        shared.deafened.store(true, Ordering::Relaxed);
        ready.clear();
        fill(&shared, 480, 1.0, &mut scratch, &mut ready);
        assert!(ready.iter().all(|s| *s == 0.0));
    }

    /// The point of carrying stereo all the way through: a screen share's audio keeps its channels.
    #[test]
    fn a_stereo_stream_keeps_its_two_channels_through_the_mixer() {
        let shared = Shared::new(&settings());
        let slot = shared.slot_for(1).unwrap();
        // Hard left: L at 0.8, R at zero.
        let interleaved: Vec<f32> =
            (0..4_000).flat_map(|_| [0.8f32, 0.0].into_iter()).collect();
        slot.ring.push(&interleaved);
        slot.primed.store(true, Ordering::Release);

        let mut scratch = Scratch::new(VOICE_SAMPLE_RATE);
        let mut ready = std::collections::VecDeque::new();
        fill(&shared, 480, 1.0, &mut scratch, &mut ready);

        assert!((ready[0] - 0.8).abs() < 1e-4, "left should be loud: {}", ready[0]);
        assert!(ready[1].abs() < 1e-4, "right should be silent: {}", ready[1]);
    }

    #[test]
    fn a_mono_device_gets_the_average_rather_than_the_left_channel() {
        // A laptop with one speaker must not lose whatever a share panned right.
        let mut mono = [0.0; 1];
        write_frame(1.0, 0.0, &mut mono);
        assert_eq!(mono[0], 0.5);

        let mut stereo = [0.0; 2];
        write_frame(1.0, -1.0, &mut stereo);
        assert_eq!(stereo, [1.0, -1.0]);

        // More channels than two: the extras are centred rather than left silent.
        let mut surround = [0.0; 4];
        write_frame(1.0, 0.0, &mut surround);
        assert_eq!(surround, [1.0, 0.0, 0.5, 0.5]);

        // And a zero-channel frame is not a panic.
        write_frame(1.0, 1.0, &mut []);
    }

    /// A speaker that has not buffered its jitter allowance must be silent rather than stuttering.
    #[test]
    fn an_unprimed_speaker_is_not_played() {
        let shared = Shared::new(&settings());
        let slot = shared.slot_for(1).unwrap();
        slot.ring.push(&vec![0.5; 8_000]);
        // Deliberately not primed.

        let mut scratch = Scratch::new(VOICE_SAMPLE_RATE);
        let mut ready = std::collections::VecDeque::new();
        fill(&shared, 480, 1.0, &mut scratch, &mut ready);
        assert!(ready.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn an_underrun_re_primes_instead_of_limping_on() {
        let shared = Shared::new(&settings());
        let slot = shared.slot_for(1).unwrap();
        // Less than one callback's worth.
        slot.ring.push(&[0.5; 100]);
        slot.primed.store(true, Ordering::Release);

        let mut scratch = Scratch::new(VOICE_SAMPLE_RATE);
        let mut ready = std::collections::VecDeque::new();
        fill(&shared, 480, 1.0, &mut scratch, &mut ready);

        assert!(!slot.primed.load(Ordering::Acquire), "it should wait for a buffer again");
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
        // And what there *was* still played, rather than being thrown away.
        assert!(ready.iter().take(100).any(|s| *s > 0.4));
    }

    /// Per-person volume only works if slots have owners, and the packets do not carry one — the
    /// control plane does. Before `attribute` existed, every slot's owner was zero and setting a
    /// volume silently matched nothing at all.
    #[test]
    fn a_stream_learns_its_owner_from_the_control_plane() {
        let shared = Shared::new(&settings());
        let slot = shared.slot_for(5).unwrap();
        assert_eq!(slot.user.load(Ordering::Relaxed), 0, "unattributed to begin with");

        // What `VoiceSession::attribute` does, on a slot that already exists.
        slot.user.store(42, Ordering::Relaxed);
        assert_eq!(slot.user.load(Ordering::Relaxed), 42);

        // And on one that does not: the answer is remembered for when the first packet arrives.
        if let Ok(mut owners) = shared.owners.lock() {
            owners.insert(9, Id(7));
        }
        let later = shared.slot_for(9).unwrap();
        assert_eq!(later.user.load(Ordering::Relaxed), 7, "attributed from its first packet");
    }

    /// Opus at the settings this app uses, round-tripped: a 20 ms frame in, a 20 ms frame out, and
    /// the encoder actually producing something.
    #[test]
    fn a_voice_frame_survives_opus() {
        let mut encoder =
            opus::Encoder::new(VOICE_SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
                .unwrap();
        encoder.set_bitrate(opus::Bitrate::Bits(VOICE_BITRATE)).unwrap();
        let mut decoder = opus::Decoder::new(VOICE_SAMPLE_RATE, opus::Channels::Mono).unwrap();

        let input: Vec<f32> = (0..VOICE_FRAME_SAMPLES)
            .map(|i| 0.3 * (i as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin())
            .collect();

        let mut packet = vec![0u8; 4_000];
        let length = encoder.encode_float(&input, &mut packet).unwrap();
        assert!(length > 0, "the encoder produced nothing");
        assert!(
            length + boa_proto::media::HEADER_LEN + boa_proto::media::TAG_LEN
                <= boa_proto::media::MAX_DATAGRAM,
            "a voice packet must fit one datagram: {length} bytes"
        );

        let mut out = vec![0.0f32; VOICE_FRAME_SAMPLES * 2];
        let samples = decoder.decode_float(&packet[..length], &mut out, false).unwrap();
        assert_eq!(samples, VOICE_FRAME_SAMPLES);

        // Loss concealment produces a frame from nothing, which is what the receive loop relies on —
        // and it fills *the buffer it is given*, which is the trap. Handed a four-frame scratch
        // buffer it invents four frames, so the receive loop has to slice it down to one.
        let mut one_frame = vec![0.0f32; VOICE_FRAME_SAMPLES];
        let concealed = decoder.decode_float(&[], &mut one_frame, false).unwrap();
        assert_eq!(concealed, VOICE_FRAME_SAMPLES);

        let stretched = decoder.decode_float(&[], &mut out, false).unwrap();
        assert_eq!(
            stretched,
            out.len(),
            "concealment fills the buffer; the receive loop must pass exactly one frame"
        );
    }
}
