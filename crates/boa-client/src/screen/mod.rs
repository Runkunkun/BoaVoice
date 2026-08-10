//! Sharing a screen, and watching somebody else's.
//!
//! ```text
//!  ffmpeg (capture, scale, encode) ──stdout──▶ NAL units ──▶ fragment ──▶ seal ──▶ UDP
//!                                            UDP ──▶ open ──▶ reassemble ──▶ openh264 ──▶ texture
//! ```
//!
//! **Why ffmpeg for the sending side.** Screen capture is the one job in this app with no portable
//! Rust answer: it is ScreenCaptureKit on macOS, X11 or PipeWire on Linux, and DXGI on Windows, each
//! with its own permission model. The crate that wraps all three could not be used here — it pulls a
//! second, incompatible ALSA binding and collides with the audio stack — and writing three backends
//! by hand is a project of its own. ffmpeg already does it, on all three, and while it is there it
//! also does the scaling and the H.264 encoding with hardware acceleration. RedPython leans on ffmpeg
//! for the same sort of reason, so the family precedent is set.
//!
//! The cost is honest and worth stating: **sharing a screen needs ffmpeg installed. Watching one does
//! not.** The decoder is [`openh264`], in-process, built from source with the crate — so somebody who
//! only ever watches needs nothing but the app.
//!
//! **Desktop audio** travels beside the picture as a second stream — stereo Opus, on the same stream
//! id and the same socket. It needs a loopback device on macOS and Windows and nothing at all on
//! Linux; [`audio`] explains why, and says what to install when there is nothing. A machine without
//! one still shares its screen, silently, with the reason in the interface rather than in a log.

pub mod audio;
pub mod recv;
pub mod send;

pub use audio::{find_loopback, DesktopAudio, Loopback};
pub use recv::{Frame, Watcher};
pub use send::{ffmpeg_available, Share};

/// The largest picture the sender will produce, whatever the settings say.
///
/// 4K. Not a quality policy — the settings go to the hardware's limit and the server has no opinion —
/// but a decoder limit: H.264 level 5.2, which is what every software decoder in practice supports,
/// stops here. A stream above it decodes as nothing on the far side, which is worse than a stream
/// that was quietly scaled down.
pub const MAX_DIMENSION: u32 = 3_840;
