//! Frosted-glass backdrop via AppKit's `NSVisualEffectView`.
//!
//! The idea is simple — put a visual-effect view behind everything the app draws
//! and let the window server blur the desktop into it — but *where* it goes took
//! two wrong answers to find, and both failed in ways worth recording.
//!
//! **Not inside the render view.** The obvious move is to add the effect view as
//! a subview of the window's content view, ordered below its siblings. That is
//! wrong here, because winit makes *its own render view* the content view: the
//! effect view then lands inside the very view it is meant to sit behind, and a
//! subview always draws above its host's layer. The result is a window filled
//! edge to edge with blurred desktop and no interface at all.
//!
//! **Not as a replacement content view either.** The textbook arrangement — make
//! the effect view the content view and re-parent the render view into it — is
//! what AppKit documents, and it crashes. winit reaches for its view by casting
//! whatever `contentView` returns:
//!
//! ```text
//! // SAFETY: The view inside WinitWindow is always `WinitView`
//! unsafe { Retained::cast(self.window().contentView().unwrap()) }
//! ```
//!
//! Displace that view and the cast is reading an `NSVisualEffectView` as a
//! `WinitView`; the next cursor change segfaults in `objc_retain`.
//!
//! So the effect view goes **beside** the render view, as an earlier sibling in
//! the window's frame view. `contentView` keeps returning exactly what winit put
//! there, and the blur still sits behind everything, because siblings earlier in
//! the subview list draw first.
//!
//! Two settings have to agree or the effect collapses regardless: the window must
//! be non-opaque with a clear background, and the wgpu surface must be
//! transparent (`ViewportBuilder::with_transparent`) so our own draw does not
//! cover it.
//!
//! Everything here is best-effort. Each step that can fail logs and returns, the
//! result is checked afterwards, and a wrong hierarchy is torn back out rather
//! than left in place — an invisible interface is far worse than a flat one.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameVibrantDark,
    NSAutoresizingMaskOptions, NSColor, NSUserInterfaceItemIdentification as _, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindow, NSWindowOrderingMode,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Marks the view we inserted, so a second call replaces rather than stacks.
///
/// AppKit has no user-data slot on a view, and subclassing to add one would drag
/// in a whole class declaration. The tag rides on the view's `identifier`, which
/// AppKit itself only uses for nib lookups.
const TAG: &str = "boavoice.vibrancy";

pub fn install_vibrancy(window: &dyn HasWindowHandle) {
    // An escape hatch, because the failure mode this guards against is total: a
    // window showing nothing but blurred desktop is not a window you can use to
    // turn the effect off. `BOAVOICE_NO_VIBRANCY=1` starts with plain
    // translucency instead.
    if std::env::var_os("BOAVOICE_NO_VIBRANCY").is_some() {
        log::info!("vibrancy: disabled by BOAVOICE_NO_VIBRANCY");
        return;
    }

    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("vibrancy: not on the main thread, skipping");
        return;
    };

    let handle = match window.window_handle() {
        Ok(handle) => handle,
        Err(err) => {
            log::warn!("vibrancy: no window handle ({err})");
            return;
        }
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        log::warn!("vibrancy: window handle is not AppKit");
        return;
    };

    // SAFETY: winit hands out an `NSView*` in `ns_view` and keeps it alive for as
    // long as the window exists. We only borrow it for the length of this call,
    // and we are on the main thread, which is where AppKit requires view access.
    let render_view: &NSView = unsafe { appkit.ns_view.cast::<NSView>().as_ref() };

    let Some(window) = render_view.window() else {
        log::warn!("vibrancy: view is not in a window yet");
        return;
    };
    let Some(content) = window.contentView() else {
        log::warn!("vibrancy: window has no content view");
        return;
    };

    // The window's frame view: the content view's parent, which also owns the
    // title bar. Reached through the public `superview` accessor rather than by
    // naming the private class, and only public `NSView` methods are called on
    // it.
    // SAFETY: reading a view's superview on the main thread is sound; objc2
    // marks it unsafe only because a `superview` override could, in principle,
    // return something that is not a view.
    let Some(frame) = (unsafe { content.superview() }) else {
        log::warn!("vibrancy: content view has no superview yet");
        return;
    };

    if find_tagged(&frame).is_some() {
        // Already installed. Re-inserting would stack a second full-window blur.
        return;
    }

    // The blur is drawn by the window server *behind* the window's own backing
    // store, so the window has to stop claiming it fills its frame opaquely.
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));

    // Vibrant-dark rather than plain dark: it tells AppKit the content above the
    // blur is light-on-dark, and the material's tone mapping adjusts so bright
    // desktops behind the window do not wash the text out.
    if let Some(appearance) = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameVibrantDark }) {
        window.setAppearance(Some(&appearance));
    }

    let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame.bounds());

    // `UnderWindowBackground` is the material AppKit itself uses for full-window
    // backdrops (the one behind Finder and Music). `HudWindow` is the tempting
    // alternative but it is tuned for small floating panels and reads far too
    // dark once it covers a whole window.
    effect.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
    // Blur what is *behind the window*, not what is behind the view within it.
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    // Keep the frost when the app loses focus. The default drops to a flat fill
    // on deactivation, which makes the whole UI visibly lurch when you click away
    // — jarring for a player that spends most of its life in the background.
    effect.setState(NSVisualEffectState::Active);
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    effect.setIdentifier(Some(&objc2_foundation::NSString::from_str(TAG)));

    // Below the content view, as its sibling. `contentView` is left pointing at
    // winit's view, which is the whole reason this is an insertion rather than a
    // re-parenting.
    frame.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, Some(&content));

    if !verify(&window) {
        log::error!("vibrancy: rolling back to plain translucency");
        effect.removeFromSuperview();
        window.setOpaque(true);
    }
}

/// Our effect view among `frame`'s subviews, if it is already there.
fn find_tagged(frame: &NSView) -> Option<Retained<NSVisualEffectView>> {
    frame.subviews().into_iter().find_map(|view| {
        if !view.identifier().is_some_and(|id| id.to_string() == TAG) {
            return None;
        }
        // The tag is ours, so the class is ours too; the downcast only guards
        // against a future where something else reuses the identifier.
        Retained::downcast::<NSVisualEffectView>(view).ok()
    })
}

/// Check that the effect really ended up behind the interface.
///
/// Worth doing at runtime rather than trusting the code above, because getting it
/// wrong is invisible from inside the app: the renderer keeps drawing perfectly
/// into a surface that nothing can see, so every check short of looking at the
/// window still passes. Both wrong arrangements this module has been through
/// produced a blank window and no error anywhere.
fn verify(window: &NSWindow) -> bool {
    let Some(content) = window.contentView() else {
        log::error!("vibrancy: window lost its content view");
        return false;
    };

    // winit casts `contentView` to its own type without checking, so anything
    // else there is a crash waiting for the next cursor change.
    if Retained::downcast::<NSVisualEffectView>(content.clone()).is_ok() {
        log::error!("vibrancy: the effect view displaced the render view");
        return false;
    }

    // SAFETY: as above — main thread, and AppKit's own frame view.
    let Some(frame) = (unsafe { content.superview() }) else {
        log::error!("vibrancy: content view has no superview");
        return false;
    };

    let subviews = frame.subviews();
    let position = |needle: &NSView| {
        subviews
            .iter()
            .position(|view| std::ptr::eq(&*view as *const NSView, needle as *const NSView))
    };

    let Some(effect) = find_tagged(&frame) else {
        log::error!("vibrancy: the effect view is not in the window");
        return false;
    };
    let (Some(effect_at), Some(content_at)) = (position(&effect), position(&content)) else {
        log::error!("vibrancy: could not locate the views in the frame");
        return false;
    };

    // Earlier in the subview list means drawn first, which means behind.
    if effect_at < content_at {
        log::debug!("vibrancy: effect at {effect_at}, interface at {content_at}");
        true
    } else {
        log::error!("vibrancy: effect at {effect_at} is in front of the interface at {content_at}");
        false
    }
}
