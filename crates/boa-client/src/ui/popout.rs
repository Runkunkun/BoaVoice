//! The shared screen in a window of its own.
//!
//! Watching somebody's screen inside a chat window means watching it in a third of a screen. Discord
//! calls this popping out; the picture goes into a plain window that can be moved to another display,
//! made full screen, or left in a corner while you work — which is what somebody watching a share
//! actually wants to do with it.
//!
//! The mechanism is egui's **deferred viewport**: a second operating-system window, drawn by a closure
//! that egui calls when it needs to. Two consequences shape everything here.
//!
//! **The closure cannot borrow the app.** It has to be `Send + Sync + 'static`, because egui may call it
//! from outside the frame that created it. So what the window needs to draw is copied into a shared box
//! once per frame, and the window reads only that. There is no path by which the popped-out window can
//! touch application state, which also means there is no way for it to deadlock against the audio
//! callbacks.
//!
//! **The texture is shared, not copied.** Textures live in the egui context rather than in a window, so
//! the popped-out window draws the *same* texture the main window would have drawn: a 4K frame is
//! uploaded once and appears in whichever window is showing it. Popping out costs nothing per frame.

use std::sync::{Arc, Mutex};

use crate::theme;

/// What the popped-out window draws, refreshed once per frame by the main window.
#[derive(Default)]
pub struct Shown {
    /// The picture. `None` while waiting for the first frame, which the window says out loud rather
    /// than showing an empty black rectangle.
    pub texture: Option<egui::TextureHandle>,
    /// Whose screen this is, for the title bar.
    pub whose: String,
    /// Cleared when the window is closed, by its own button or by the one in the app.
    pub open: bool,
    /// A line under the picture when something is wrong — loss, or a share that has stopped.
    pub trouble: Option<String>,
}

/// A handle on the popped-out window, held by the app.
#[derive(Clone, Default)]
pub struct PopOut(Arc<Mutex<Shown>>);

impl PopOut {
    /// Whether the window should exist at all.
    pub fn open(&self) -> bool {
        self.0.lock().map(|shown| shown.open).unwrap_or(false)
    }

    /// Open it, for this person's screen.
    pub fn open_for(&self, whose: String) {
        if let Ok(mut shown) = self.0.lock() {
            shown.whose = whose;
            shown.open = true;
            // Not the texture: whatever was last uploaded is still valid, and clearing it would make the
            // window flash "waiting for a picture" for one frame every time it is opened.
        }
    }

    /// Close it.
    pub fn close(&self) {
        if let Ok(mut shown) = self.0.lock() {
            shown.open = false;
        }
    }

    /// Give the window this frame's picture and status.
    pub fn update(
        &self,
        texture: Option<&egui::TextureHandle>,
        whose: &str,
        trouble: Option<String>,
    ) {
        if let Ok(mut shown) = self.0.lock() {
            shown.texture = texture.cloned();
            shown.whose = whose.to_string();
            shown.trouble = trouble;
        }
    }

    /// Draw the window, if it is open. Called once per frame from the main window.
    ///
    /// Returns whether it is still open, so the caller can put the picture back in the app when the
    /// window has been closed with its own close button.
    pub fn show(&self, ctx: &egui::Context) -> bool {
        if !self.open() {
            return false;
        }
        let shown = self.0.clone();
        let title = self
            .0
            .lock()
            .map(|shown| format!("{} — BoaVoice", shown.whose))
            .unwrap_or_else(|_| "BoaVoice".to_string());

        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("boa-screen-popout"),
            egui::ViewportBuilder::default()
                .with_title(title)
                // 720p, which is a window rather than a takeover; the point is that it can then be
                // resized, moved to another display or made full screen by the window manager.
                .with_inner_size([1_280.0, 720.0])
                .with_min_inner_size([320.0, 180.0]),
            move |ctx, _class| {
                // A frame of its own rather than the app's glass: this window has no chrome to match,
                // and a shared picture reads best against something that is not competing with it.
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::mocha::CRUST))
                    .show(ctx, |ui| {
                        let Ok(shown) = shown.lock() else { return };
                        draw(ui, &shown);
                    });

                // The window's own close button. Handled here rather than by egui, because the app has
                // to learn about it: the picture goes back into the main window.
                if ctx.input(|input| input.viewport().close_requested()) {
                    if let Ok(mut shown) = shown.lock() {
                        shown.open = false;
                    }
                }
            },
        );
        true
    }
}

/// The window's contents: the picture, scaled to fit, and a word when there is none.
fn draw(ui: &mut egui::Ui, shown: &Shown) {
    let rect = ui.available_rect_before_wrap();
    match shown.texture.as_ref() {
        Some(texture) => {
            let size = texture.size_vec2();
            // Fitted rather than filled, and never enlarged past the source: a 720p share blown up to
            // fill a 4K window is a blurry mess, and letterboxing is what everybody expects.
            let scale = (rect.width() / size.x).min(rect.height() / size.y);
            let drawn = egui::Rect::from_center_size(rect.center(), size * scale);
            ui.painter().image(
                texture.id(),
                drawn,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        None => {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "waiting for a picture…",
                egui::FontId::proportional(13.0),
                theme::TEXT_FAINT,
            );
        }
    }

    if let Some(trouble) = shown.trouble.as_ref() {
        // Over the picture rather than beside it, because the window is the picture: there is no
        // sidebar to put a status line in, and a line that shrinks the video would be worse.
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() - 14.0),
            egui::Align2::CENTER_BOTTOM,
            trouble,
            egui::FontId::proportional(11.0),
            theme::WARN,
        );
    }
}
