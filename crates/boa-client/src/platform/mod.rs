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

/// Whether the app may capture the screen, and asking for it if not.
///
/// macOS gates screen capture behind a permission the *app* has to ask for, and asking is a real API
/// call rather than a plist entry: `CGRequestScreenCaptureAccess` shows the system prompt, and
/// `CGPreflightScreenCaptureAccess` answers without showing anything. Two plain C functions in
/// CoreGraphics — no Objective-C object involved — which is why this is here rather than in a
/// framework binding.
///
/// The awkward part is not the call, it is what "granted" means: macOS caches the answer per process,
/// so an app that has just been granted the permission still cannot capture until it is **restarted**.
/// That is not a bug to work around, it is a fact to tell the user, which is why this returns three
/// states rather than a bool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenAccess {
    /// Allowed, and capture will work now.
    Granted,
    /// The prompt has just been shown, or the permission was granted after this process started.
    /// Either way capture will not work until the app is restarted.
    AskedForIt,
    /// Refused, or the platform has no such notion — capture may still work, or may not.
    Unknown,
}

/// Whether screen capture is allowed, without asking for anything.
///
/// Separate from [`request_screen_access`] because the two are wanted in different places: this one is
/// for the log line at start-up, where showing a permission dialogue would be rude and where the answer
/// is the single most useful thing to know when a share later fails.
pub fn screen_access_granted() -> bool {
    #[cfg(target_os = "macos")]
    // SAFETY: takes no arguments, returns a plain `bool`, present since 10.15.
    unsafe {
        CGPreflightScreenCaptureAccess()
    }
    #[cfg(not(target_os = "macos"))]
    true
}

/// Check the permission, and ask for it if it has not been granted.
///
/// Safe to call repeatedly: the preflight is cheap and the prompt is shown by the system at most once
/// per install, whatever this does.
pub fn request_screen_access() -> ScreenAccess {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: both take no arguments, return a plain `bool`, and are present since 10.15 — well
        // below this app's minimum. They are the documented way to ask.
        unsafe {
            if CGPreflightScreenCaptureAccess() {
                return ScreenAccess::Granted;
            }
            // Shows the system prompt if it has not been shown before. The return value is what the
            // *current* process is allowed to do, which right after granting is still nothing.
            let now = CGRequestScreenCaptureAccess();
            log::info!("screen: asked for capture access; granted to this process: {now}");
            crate::diagnostics::note(&format!("screen: access requested, granted now: {now}"));
            if now {
                ScreenAccess::Granted
            } else {
                ScreenAccess::AskedForIt
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // X11 has no such gate, Windows has none for the desktop, and Wayland asks through a portal at
        // capture time rather than in advance.
        ScreenAccess::Unknown
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Whether this platform has a frosted backdrop to install at all.
///
/// Used by the settings screen to explain the window's appearance rather than leaving
/// somebody on Linux wondering why theirs looks flatter than the screenshots.
pub fn has_vibrancy() -> bool {
    cfg!(target_os = "macos")
}
