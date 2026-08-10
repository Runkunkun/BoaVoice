//! Sharing a screen, and watching somebody else's.
//!
//! ```text
//!  macOS:  ScreenCaptureKit ──▶ VideoToolbox ──┐
//!  else:   ffmpeg ──stdout──▶ Annex-B parser ──┴─▶ fragment ──▶ seal ──▶ UDP
//!                                            UDP ──▶ open ──▶ reassemble ──▶ openh264 ──▶ texture
//! ```
//!
//! **Two engines, and the source decides which.** Screen capture is the one job in this app with no
//! portable Rust answer: it is ScreenCaptureKit on macOS, X11 or PipeWire on Linux, and DXGI on
//! Windows, each with its own permission model. The crate that wraps all three could not be used here
//! — it pulls a second, incompatible ALSA binding and collides with the audio stack.
//!
//! On **macOS** it is done in-process, in [`mac`]: ScreenCaptureKit for the capture and VideoToolbox
//! for the H.264, both part of the operating system. That is what makes a *single window* shareable at
//! all, what lets the machine's own sound travel with the picture, and what means a Mac needs nothing
//! installed. Everywhere else — and on a Mac where the framework cannot be reached — it is **ffmpeg**,
//! which also does the scaling and the encoding with hardware acceleration. RedPython leans on ffmpeg
//! for the same sort of reason, so the family precedent is set.
//!
//! The cost is honest and worth stating: **sharing a screen on Linux or Windows needs ffmpeg
//! installed. Watching one never does.** The decoder is [`openh264`], in-process, built from source
//! with the crate — so somebody who only ever watches needs nothing but the app.
//!
//! **Desktop audio** travels beside the picture as a second stream — stereo Opus, on the same stream
//! id and the same socket. On macOS it is a second output of the same capture, which is how it avoids
//! the loopback device that every other route needs; elsewhere it is a *device* being recorded, a
//! PulseAudio monitor source on Linux and a virtual cable on Windows. [`audio`] explains what and why,
//! and says what to install when there is nothing to record.
//!
//! A machine that can do neither still shares its screen, silently, with the reason in the interface
//! rather than in a log.

pub mod audio;
pub mod ffmpeg;
#[cfg(target_os = "macos")]
pub mod mac;
pub mod recv;
pub mod send;

pub use audio::{find_loopback, DesktopAudio, Loopback};
pub use recv::{Feed, Frame, Watcher};
pub use ffmpeg::available as ffmpeg_available;
pub use send::{sources, Share, Source};

/// Whether this platform captures a screen in-process, with nothing installed.
///
/// Used before a share is announced, to decide whether a missing ffmpeg is worth stopping for. On macOS
/// it is not: the capture is [`mac`]'s, and ffmpeg is only the fallback for a machine where the
/// framework cannot be reached — which reports itself when it happens.
pub fn native_capture() -> bool {
    cfg!(target_os = "macos")
}

/// The largest picture the sender will produce, whatever the settings say.
///
/// 4K. Not a quality policy — the settings go to the hardware's limit and the server has no opinion —
/// but a decoder limit: H.264 level 5.2, which is what every software decoder in practice supports,
/// stops here. A stream above it decodes as nothing on the far side, which is worse than a stream
/// that was quietly scaled down.
pub const MAX_DIMENSION: u32 = 3_840;
