//! Capturing a screen the way macOS wants it done.
//!
//! This is the replacement for shelling out to ffmpeg on macOS, and it exists for three reasons that
//! all point the same way:
//!
//! * **A single window.** ffmpeg's avfoundation input captures a display and nothing smaller.
//!   ScreenCaptureKit captures a window, an application, or a display with windows excluded.
//! * **The machine's own sound**, with no loopback device to install — under the same
//!   screen-recording permission a share already needs.
//! * **Nothing to ship.** Both frameworks are part of the operating system. The alternative was
//!   bundling a static ffmpeg: 76 MB, GPL, and the readily available macOS build is x86-only, so it
//!   would run under Rosetta on every current Mac.
//!
//! It is hand-written FFI over `objc2`, deliberately rather than through the ergonomic wrapper crate
//! for ScreenCaptureKit: that one builds Swift helper libraries, which makes a full Xcode install a
//! build requirement for anybody compiling this app.

pub mod capture;
pub mod content;
pub mod encode;
