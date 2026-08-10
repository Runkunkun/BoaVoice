//! Asking macOS what there is to share.
//!
//! `SCShareableContent` is the list of displays, windows and applications the user is allowed to
//! capture. It replaces the ffmpeg device listing, and the difference is the point: ffmpeg's
//! avfoundation input offers *displays* and nothing smaller, so a single window was not something this
//! app could offer at all. Here it is one entry in the same list.
//!
//! Two awkward things about the API, both dealt with here rather than pushed at the caller.
//!
//! **It is asynchronous.** The only way to get the content is a completion handler, which arrives on
//! some queue of the framework's choosing. The interface asks this question when somebody presses a
//! button and wants an answer before drawing the next frame, so this blocks on a channel with a
//! timeout — a few tens of milliseconds in practice, and a bounded wait rather than a hang if the
//! window server is busy.
//!
//! **It is the permission gate.** With screen recording not granted, the call fails rather than
//! returning an empty list. That is useful: it is the earliest and clearest place to find out, well
//! before anything has been announced to the other people in the call.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayCopyDisplayMode, CGDisplayMode, CGError, CGGetDisplaysWithPoint, CGMainDisplayID,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{SCDisplay, SCShareableContent, SCWindow};

use crate::screen::Source;

/// How long to wait for the window server to answer.
///
/// Two seconds. The call normally takes tens of milliseconds; anything approaching this means the
/// window server is wedged or a permission dialogue is in front of it, and in both cases returning
/// with an explanation beats blocking the interface.
const PATIENCE: Duration = Duration::from_secs(2);

/// What macOS says can be captured.
pub struct Shareable {
    pub displays: Retained<NSArray<SCDisplay>>,
    pub windows: Retained<NSArray<SCWindow>>,
}

/// Ask for the current shareable content, blocking briefly.
pub fn shareable() -> Result<Shareable, String> {
    let (tx, rx) = mpsc::sync_channel::<Result<Shareable, String>>(1);

    // The completion block. It is called once, on a framework queue, and `RcBlock` keeps it alive
    // until then.
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: the framework hands over either a content object or an error, both borrowed for the
        // duration of the call. Nothing is retained past it except the arrays, which are retained by
        // the accessors below.
        let answer = unsafe {
            if let Some(content) = content.as_ref() {
                Ok(Shareable { displays: content.displays(), windows: content.windows() })
            } else if let Some(error) = error.as_ref() {
                Err(describe(error))
            } else {
                Err("ScreenCaptureKit returned neither content nor an error".to_string())
            }
        };
        // A failed send means the caller gave up waiting, which is not worth reporting: it already
        // said so.
        let _ = tx.send(answer);
    });

    // SAFETY: the class method takes the block and returns immediately; the block is invoked once.
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&handler);
    }

    match rx.recv_timeout(PATIENCE) {
        Ok(answer) => answer,
        Err(_) => Err(
            "ScreenCaptureKit did not answer within two seconds — the window server may be busy, \
             or a permission dialogue may be waiting"
                .to_string(),
        ),
    }
}

/// Read an `NSError` the way a person would want it.
///
/// The code matters as much as the message: `SCStreamErrorUserDeclined` (-3801) is the one that means
/// "refused in System Settings", and it reads as an ordinary failure otherwise.
fn describe(error: &NSError) -> String {
    let code = error.code();
    let text = error.localizedDescription().to_string();
    match code {
        -3801 => "screen recording was refused. System Settings → Privacy & Security → \
                  Screen & System Audio Recording → BoaVoice, then restart the app."
            .to_string(),
        _ => format!("{text} (code {code})"),
    }
}

/// Everything shareable, as the picker's list.
///
/// Screens first, then windows with a title, largest first. The ordering is the whole reason this
/// function exists rather than the caller walking the arrays: a raw window list is dozens of entries
/// including every menu extra and the desktop, in whatever order the window server keeps them, and
/// that is not a list anybody can choose from.
pub fn sources() -> Result<Vec<Source>, String> {
    let content = shareable()?;
    let mut found = Vec::new();

    for (index, display) in content.displays.iter().enumerate() {
        // SAFETY: reading properties of a display the framework just handed over.
        let (width, height) = unsafe { (display.width(), display.height()) };
        let id = unsafe { display.displayID() };
        found.push(Source {
            input: format!("display:{id}"),
            label: if index == 0 {
                format!("Main screen ({width}×{height})")
            } else {
                format!("Screen {} ({width}×{height})", index + 1)
            },
            window: false,
        });
    }

    let mut windows: Vec<(i64, Source)> = Vec::new();
    for window in content.windows.iter() {
        // SAFETY: as above — plain property reads on a framework object.
        let (title, app, bundle, on_screen, width, height, id) = unsafe {
            let owner = window.owningApplication();
            (
                window.title().map(|t| t.to_string()).unwrap_or_default(),
                // `title` is optional and `applicationName` is not, which is the sort of asymmetry
                // that only shows up when the compiler objects.
                owner.as_ref().map(|app| app.applicationName().to_string()),
                owner.as_ref().map(|app| app.bundleIdentifier().to_string()),
                window.isOnScreen(),
                window.frame().size.width as i64,
                window.frame().size.height as i64,
                window.windowID(),
            )
        };

        // What to leave out, and why — because the raw list is mostly things nobody would ever choose.
        //
        // No title: a shadow, a tooltip, a status item. Not on screen: its own owner cannot see it.
        // Smaller than a dialogue: furniture. **No owning application at all**: the window server's
        // own scaffolding, which is where entries like "Display 3 Backstop" come from.
        // An *empty* application name counts as none: the window server's helpers have an owning
        // application whose name is the empty string, which is where entries like "Display 3 Backstop"
        // and "underbelly" come from. Asking only whether the owner exists lets both through.
        let (Some(app), Some(bundle)) = (app, bundle) else { continue };
        if app.trim().is_empty() || title.trim().is_empty() || !on_screen || width < 120 || height < 80
        {
            continue;
        }
        if is_furniture(&bundle) {
            continue;
        }

        windows.push((
            width * height,
            Source { input: format!("window:{id}"), label: format!("{app} — {title}"), window: true },
        ));
    }
    // Largest first: the window somebody means to share is almost always a big one, and the long tail
    // of small utility windows belongs at the bottom of the list.
    windows.sort_by_key(|(area, _)| std::cmp::Reverse(*area));
    found.extend(windows.into_iter().map(|(_, source)| source));

    Ok(found)
}

/// Whether a bundle identifier belongs to the desktop rather than to an application.
///
/// Matched on the identifier rather than the name: the names are localised, so a German system offers
/// "Mitteilungszentrale" and an English one "Notification Center", and a filter on names would let one
/// of them through. Every one of these appears in the window list on a stock machine, and not one of
/// them is something somebody means to share.
fn is_furniture(bundle: &str) -> bool {
    const DESKTOP: [&str; 8] = [
        "com.apple.dock",
        "com.apple.notificationcenterui",
        "com.apple.WindowManager",
        "com.apple.wallpaper",
        "com.apple.controlcenter",
        "com.apple.systemuiserver",
        "com.apple.WindowServer",
        "com.apple.screencaptureui",
    ];
    DESKTOP.contains(&bundle)
}

/// What a [`Source`]'s `input` refers to.
///
/// The string form exists because a `Source` travels through the interface, which has no business
/// holding a framework object — and because the ffmpeg path uses the same type with a device index in
/// the same field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Display(u32),
    Window(u32),
}

/// A target as the framework object needed to capture it, and how big it really is.
pub enum Located {
    Display(Retained<SCDisplay>),
    Window(Retained<SCWindow>),
}

/// Find a target in the *current* shareable content.
///
/// Looked up again rather than held from the picker, because between choosing a window and pressing
/// share it may have been closed — and a stale `SCWindow` produces a stream that starts and delivers
/// nothing, which is the worst of the failure modes because it looks like it worked.
pub fn locate(target: Target) -> Result<Located, String> {
    let content = shareable()?;
    match target {
        Target::Display(id) => content
            .displays
            .iter()
            // SAFETY: a property read on a framework object.
            .find(|display| unsafe { display.displayID() } == id)
            .map(Located::Display)
            .ok_or_else(|| format!("screen {id} is no longer there")),
        Target::Window(id) => content
            .windows
            .iter()
            // SAFETY: as above.
            .find(|window| unsafe { window.windowID() } == id)
            .map(Located::Window)
            .ok_or_else(|| "that window has been closed".to_string()),
    }
}

impl Located {
    /// The size to capture at, in **pixels**.
    ///
    /// ScreenCaptureKit reports sizes in *points*, and a stream configured with those numbers captures
    /// a Retina screen at half its resolution — a picture that is not broken, just soft, which is the
    /// sort of wrong that gets shipped. The backing scale is not on the framework object either, so it
    /// comes from the display mode: pixel width over point width.
    pub fn pixels(&self) -> (u32, u32) {
        match self {
            // SAFETY: property reads on a framework object.
            Located::Display(display) => unsafe {
                let scale = scale(display.displayID());
                let (width, height) = (display.width() as f64, display.height() as f64);
                ((width * scale) as u32, (height * scale) as u32)
            },
            // SAFETY: as above.
            Located::Window(window) => unsafe {
                let frame = window.frame();
                // Which display the window is on decides its scale: a window dragged onto a
                // non-Retina second monitor is captured at 1×, and asking for 2× would be upscaling
                // blur at twice the bitrate.
                let scale = scale(display_at(CGPoint {
                    x: frame.origin.x + frame.size.width / 2.0,
                    y: frame.origin.y + frame.size.height / 2.0,
                }));
                ((frame.size.width * scale) as u32, (frame.size.height * scale) as u32)
            },
        }
    }

    /// What to call this in a log line.
    pub fn label(&self) -> String {
        match self {
            // SAFETY: property reads on a framework object.
            Located::Display(display) => format!("display {}", unsafe { display.displayID() }),
            Located::Window(window) => unsafe {
                window.title().map(|title| title.to_string()).unwrap_or_else(|| "a window".into())
            },
        }
    }
}

/// A display's backing scale — 2.0 on Retina, 1.0 on everything else.
fn scale(display: CGDirectDisplayID) -> f64 {
    let Some(mode) = CGDisplayCopyDisplayMode(display) else { return 1.0 };
    let points = CGDisplayMode::width(Some(&mode)) as f64;
    let pixels = CGDisplayMode::pixel_width(Some(&mode)) as f64;
    if points > 0.0 && pixels > 0.0 {
        // Clamped because a bad answer here is a stream at the wrong size: a zero would divide the
        // picture away and a huge one would ask the encoder for something it will refuse.
        (pixels / points).clamp(1.0, 3.0)
    } else {
        1.0
    }
}

/// Which display a point is on, falling back to the main one.
fn display_at(point: CGPoint) -> CGDirectDisplayID {
    let mut displays: [CGDirectDisplayID; 1] = [0];
    let mut count: u32 = 0;
    // SAFETY: the array holds one id and `max_displays` says so; both out-pointers are stack slots.
    unsafe {
        let status = CGGetDisplaysWithPoint(point, 1, displays.as_mut_ptr(), &mut count);
        if status == CGError::Success && count > 0 {
            displays[0]
        } else {
            CGMainDisplayID()
        }
    }
}

/// Read a target back out of a source.
pub fn target(source: &Source) -> Option<Target> {
    if let Some(id) = source.input.strip_prefix("display:") {
        return id.parse().ok().map(Target::Display);
    }
    if let Some(id) = source.input.strip_prefix("window:") {
        return id.parse().ok().map(Target::Window);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desktop's own windows are filtered by bundle identifier, not by name: the names are
    /// localised, and a German system says "Mitteilungszentrale".
    #[test]
    fn the_desktops_own_windows_are_left_out() {
        assert!(is_furniture("com.apple.dock"));
        assert!(is_furniture("com.apple.notificationcenterui"));
        assert!(is_furniture("com.apple.WindowManager"));
        assert!(!is_furniture("com.apple.Safari"));
        assert!(!is_furniture("org.mozilla.firefox"));
        assert!(!is_furniture("dev.boavoice.client"), "our own window is a real window");
        // A near-miss must not match: this is an equality test, not a prefix one, because
        // `com.apple.dockextra` would be somebody's utility.
        assert!(!is_furniture("com.apple.dock.extra"));
    }

    #[test]
    fn a_source_carries_which_thing_it_is() {
        let display = Source { input: "display:1".into(), label: "Main".into(), window: false };
        assert_eq!(target(&display), Some(Target::Display(1)));

        let window = Source { input: "window:4242".into(), label: "Safari".into(), window: true };
        assert_eq!(target(&window), Some(Target::Window(4242)));

        // The ffmpeg path puts a device index here, which is not a ScreenCaptureKit target and must
        // not be mistaken for one.
        let ffmpeg = Source { input: "2".into(), label: "Screen".into(), window: false };
        assert_eq!(target(&ffmpeg), None);
        assert_eq!(target(&Source { input: "display:x".into(), label: String::new(), window: false }), None);
    }

    /// Not a test of the framework — that needs a permission a test cannot grant — but of the shape of
    /// the answer: whatever happens, this returns rather than hanging, and a failure explains itself.
    #[test]
    fn asking_for_content_either_answers_or_explains() {
        match sources() {
            Ok(found) => {
                for source in &found {
                    assert!(!source.label.is_empty());
                    assert!(target(source).is_some(), "{:?} is not addressable", source.input);
                }
                // A machine with a display always has at least that.
                assert!(found.iter().any(|source| !source.window) || found.is_empty());
            }
            Err(why) => {
                assert!(!why.is_empty());
                // The refusal case has to be recognisable as such, not as a generic failure.
                if why.contains("refused") {
                    assert!(why.contains("System Settings"), "{why}");
                }
            }
        }
    }
}
