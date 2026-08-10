//! Platform hooks the portable UI cannot express.
//!
//! Currently one: **window vibrancy**. egui can paint a translucent panel, but
//! translucency alone shows the desktop *sharply* through the window, which looks like a
//! bug rather than like glass. The frosting is a compositor service, and asking for it
//! is different on every platform — and on most Linux compositors is not available at
//! all, where the app falls back to plain translucency and still looks deliberate.
//!
//! Everything here degrades to a no-op, and the app is fully usable without any of it.

#[cfg(target_os = "macos")]
mod macos;

/// Install the platform's frosted backdrop behind `window`.
///
/// Safe to call more than once; implementations are expected to be idempotent, because
/// the natural place to call it is the first frame and "the first frame" happens again
/// after the window is recreated.
///
/// Failure is never fatal — the app is fully usable with plain translucency, so problems
/// are logged rather than propagated.
#[allow(unused_variables)]
pub fn install_vibrancy(window: &dyn raw_window_handle::HasWindowHandle) {
    #[cfg(target_os = "macos")]
    macos::install_vibrancy(window);
}

/// Whether this platform has a frosted backdrop to install at all.
///
/// Used by the settings screen to explain the window's appearance rather than leaving
/// somebody on Linux wondering why theirs looks flatter than the screenshots.
pub fn has_vibrancy() -> bool {
    cfg!(target_os = "macos")
}
