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
//! id and the same socket. It is captured by recording a *device*: a PulseAudio monitor source on
//! Linux (nothing to install), or a loopback device on macOS and Windows. [`audio`] explains what and
//! why, and says what to install when there is nothing to record.
//!
//! **The better way on macOS, not done yet.** ScreenCaptureKit can hand over the machine's own output
//! directly, with no loopback device, under the same screen-recording permission a share already
//! needs — which is how Discord does it. It is not used here yet for one practical reason: the
//! ergonomic Rust wrapper for it builds Swift helper libraries and therefore needs a full Xcode
//! install, which is a heavy thing to require of anyone building this. Doing it through the raw
//! `objc2` bindings avoids that and is the intended route; it is a hand-written delegate class,
//! completion blocks and CoreMedia buffer handling, and it is the next piece of work here.
//!
//! A machine that can do neither still shares its screen, silently, with the reason in the interface
//! rather than in a log.

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
