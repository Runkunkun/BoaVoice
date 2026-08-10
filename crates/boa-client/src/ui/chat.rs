//! The message log and the composer.
//!
//! Three things here are more than they look.
//!
//! **Grouping.** Consecutive messages from the same person within a few minutes are drawn as one
//! block with one header. Without it, a back-and-forth of short lines is mostly repeated names
//! and timestamps, and the actual words are what is left over.
//!
//! **Attachments say when they expire.** Every image carries a line about how long the server
//! will still have it, and whether this machine has its own copy. That is the one place the
//! three-day storage policy becomes visible, and it has to be visible: an image that will vanish
//! from a colleague's client next week but not from yours is a thing people need to know while
//! deciding whether to save it.
//!
//! **Scroll position is defended.** A chat log that jumps because an image finished loading, or
//! that refuses to stay at the bottom when a message arrives, is the difference between a usable
//! window and an annoying one. Images reserve their final size *before* they are decoded, using
//! the dimensions the server measured at upload, so nothing shifts when the bytes land.

use egui::Ui;

use boa_proto::{Attachment, Id, Message};

use crate::state::State;
use crate::theme;
use crate::ui::{glass, icons, images, widgets, Action};

/// Messages closer together than this from the same person share a header.
const GROUP_WINDOW_MS: i64 = 5 * 60 * 1000;

/// Tallest an inline image is drawn.
///
/// A tall screenshot posted at full height pushes everything else off the screen and turns the
/// log into a scroll through one picture. Clicking opens it properly.
const MAX_IMAGE_HEIGHT: f32 = 360.0;

pub struct View<'a> {
    pub state: &'a State,
    pub images: &'a mut images::Images,
    pub channel: Id,
    pub now: boa_proto::Millis,
}

/// Draw the log. Returns an action if something in it was clicked.
pub fn log(ui: &mut Ui, view: &mut View<'_>) -> Option<Action> {
    let mut action = None;
    let state = view.state;
    let channel = view.channel;

    let scroll = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        // Sticking to the bottom is what a chat log does; egui keeps the offset from the *end*
        // when this is set, so a message arriving does not scroll away from what is being read
        // when the user has deliberately scrolled up.
        .stick_to_bottom(true);

    scroll.show(ui, |ui| {
        let log = state.log(channel);
        let messages: &[Message] = log.map(|log| log.messages.as_slice()).unwrap_or(&[]);

        // "Load older" at the top, and only when there is older to load. Explicit rather than
        // triggered by scrolling: an automatic fetch at the top competes with the user's own
        // scrolling and produces a log that fights back.
        ui.add_space(6.0);
        match log {
            Some(log) if log.loading => {
                ui.vertical_centered(|ui| {
                    ui.spinner();
                });
            }
            Some(log) if !log.complete && !messages.is_empty() => {
                ui.vertical_centered(|ui| {
                    if widgets::pill_button(ui, "Load earlier messages", false).clicked() {
                        action = Some(Action::LoadOlder(channel));
                    }
                });
            }
            Some(log) if log.complete && !messages.is_empty() => {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("the beginning of this channel")
                            .size(10.5)
                            .color(theme::TEXT_FAINT),
                    );
                });
            }
            _ => {}
        }
        ui.add_space(8.0);

        if messages.is_empty() && state.pending_for(channel).count() == 0 {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label(egui::RichText::new("Nothing here yet.").size(13.0).color(theme::TEXT_FAINT));
            });
        }

        let mut previous: Option<(Id, boa_proto::Millis)> = None;
        for message in messages {
            let grouped = previous
                .is_some_and(|(author, at)| author == message.author && message.created_at - at < GROUP_WINDOW_MS);
            if let Some(clicked) = draw_message(ui, view, message, grouped) {
                action = Some(clicked);
            }
            previous = Some((message.author, message.created_at));
        }

        // Unconfirmed sends, after everything the server has acknowledged.
        for pending in state.pending_for(channel) {
            draw_pending(ui, state, pending);
        }
        ui.add_space(8.0);
    });

    action
}

fn draw_message(ui: &mut Ui, view: &mut View<'_>, message: &Message, grouped: bool) -> Option<Action> {
    let mut action = None;
    let state = view.state;
    let mine = state.me.as_ref().is_some_and(|me| me.id == message.author);
    let label = state.label(message.author);

    ui.add_space(if grouped { 1.0 } else { 8.0 });
    ui.horizontal_top(|ui| {
        ui.add_space(4.0);
        // The avatar gutter is reserved even in a grouped message, so the text of every line in
        // a block starts at the same x. Without that the block looks ragged.
        let (gutter, _) = ui.allocate_exact_size(egui::vec2(34.0, 1.0), egui::Sense::hover());
        if !grouped {
            let at = egui::Rect::from_min_size(
                egui::pos2(gutter.left(), gutter.top()),
                egui::Vec2::splat(28.0),
            );
            let ring = state.is_speaking(message.author).then_some(theme::SPEAKING);
            widgets::avatar(ui, at, &label, message.author, ring);
        }

        ui.vertical(|ui| {
            if !grouped {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&label)
                            .size(13.0)
                            .strong()
                            .color(if mine { theme::ACCENT } else { theme::TEXT }),
                    );
                    ui.label(
                        egui::RichText::new(widgets::message_time(message.created_at, view.now))
                            .size(10.5)
                            .color(theme::TEXT_FAINT),
                    );
                    if message.edited_at.is_some() {
                        ui.label(egui::RichText::new("edited").size(10.0).color(theme::TEXT_FAINT));
                    }
                });
            }

            if !message.content.is_empty() {
                // Selectable, unlike the rest of the app: copying what somebody said is the
                // second most common thing anybody does in a chat window.
                ui.add(
                    egui::Label::new(egui::RichText::new(&message.content).size(13.5).color(theme::TEXT))
                        .selectable(true)
                        .wrap(),
                );
            }

            for attachment in &message.attachments {
                if let Some(clicked) = draw_attachment(ui, view, attachment) {
                    action = Some(clicked);
                }
            }
        });
    });

    action
}

/// One attachment: the picture if we can show it, and always the line about its life.
fn draw_attachment(ui: &mut Ui, view: &mut View<'_>, attachment: &Attachment) -> Option<Action> {
    let mut action = None;
    let have_local = crate::cache::have(&attachment.sha256);

    // Ask for the bytes if we do not have them. Once per attachment per session: the network
    // layer answers with an event, and until then the slot is `Loading`.
    if !have_local {
        action = Some(Action::WantAttachment(attachment.clone()));
    }

    ui.add_space(4.0);
    if attachment.is_image() {
        // The size comes from the metadata the server measured, so the space is reserved at the
        // right size *before* the bytes arrive. That is what stops the log from jumping as
        // images load.
        let natural = if attachment.width > 0 && attachment.height > 0 {
            egui::vec2(attachment.width as f32, attachment.height as f32)
        } else {
            egui::vec2(320.0, 180.0)
        };
        let size = images::fit(natural, ui.available_width().min(520.0), MAX_IMAGE_HEIGHT);

        match view.images.get(&attachment.sha256) {
            Some(images::Slot::Ready { texture, .. }) => {
                let response = ui.add(
                    egui::Image::new(texture)
                        .fit_to_exact_size(size)
                        .corner_radius(theme::R_CONTROL)
                        .sense(egui::Sense::click()),
                );
                if response.clicked() {
                    action = Some(Action::OpenAttachment(attachment.clone()));
                }
                response.on_hover_text(format!("{} — {}", attachment.name, widgets::bytes(attachment.size)));
            }
            Some(images::Slot::Undecodable(_)) | None => {
                placeholder(ui, size, attachment, have_local);
            }
            Some(images::Slot::Loading) => {
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                glass::well(ui, rect, theme::R_CONTROL);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "…",
                    egui::FontId::proportional(18.0),
                    theme::TEXT_FAINT,
                );
            }
        }
    } else {
        // Not an image: a row with a name and a size, which is all a file needs.
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width().min(360.0), 40.0),
            egui::Sense::click(),
        );
        glass::card(ui, rect);
        let icon = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            egui::Vec2::splat(18.0),
        );
        icons::download(ui.painter(), icon, theme::TEXT_DIM);
        ui.painter().text(
            egui::pos2(rect.left() + 38.0, rect.center().y - 6.0),
            egui::Align2::LEFT_CENTER,
            &attachment.name,
            egui::FontId::proportional(12.5),
            theme::TEXT,
        );
        ui.painter().text(
            egui::pos2(rect.left() + 38.0, rect.center().y + 8.0),
            egui::Align2::LEFT_CENTER,
            widgets::bytes(attachment.size),
            egui::FontId::proportional(10.5),
            theme::TEXT_FAINT,
        );
        if response.clicked() {
            action = Some(Action::OpenAttachment(attachment.clone()));
        }
    }

    // The line that makes the storage policy visible.
    let expiring = attachment.expires_at - view.now;
    let colour = if expiring <= 0 {
        // Past its time on the server: green when we have it (nothing is lost), red when we do
        // not (it is gone for good, and saying so plainly is better than a broken image).
        if have_local {
            theme::TEXT_FAINT
        } else {
            theme::ERROR
        }
    } else if expiring < 6 * 3_600_000 {
        theme::WARN
    } else {
        theme::TEXT_FAINT
    };
    ui.label(
        egui::RichText::new(widgets::expiry(attachment.expires_at, view.now, have_local))
            .size(10.0)
            .color(colour),
    );

    action
}

/// The box drawn where an image would be, when there is no image.
fn placeholder(ui: &mut Ui, size: egui::Vec2, attachment: &Attachment, have_local: bool) {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    glass::well(ui, rect, theme::R_CONTROL);
    let gone = !have_local && attachment.expires_at <= boa_proto::now_millis();
    let text = if gone { "gone from the server" } else { &attachment.name };
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(11.5),
        if gone { theme::ERROR } else { theme::TEXT_FAINT },
    );
    if gone {
        response.on_hover_text(
            "the server keeps attachments for three days; nobody who saved this one has it here",
        );
    }
}

/// A message we have sent and the server has not confirmed.
fn draw_pending(ui: &mut Ui, state: &State, pending: &crate::state::Pending) {
    ui.add_space(4.0);
    ui.horizontal_top(|ui| {
        ui.add_space(38.0);
        ui.vertical(|ui| {
            // Greyed while in flight, and visibly *more* grey once it has been waiting long
            // enough to look stuck. The alternative — showing it as sent — is how a message that
             // never arrived goes unnoticed.
            let colour = if pending.is_slow() { theme::WARN } else { theme::TEXT_FAINT };
            ui.add(
                egui::Label::new(egui::RichText::new(&pending.content).size(13.5).color(colour)).wrap(),
            );
            for name in &pending.attachment_names {
                ui.label(egui::RichText::new(format!("↑ {name}")).size(10.5).color(colour));
            }
            if pending.is_slow() {
                ui.label(egui::RichText::new("still sending…").size(10.0).color(theme::WARN));
            }
        });
    });
    let _ = state;
}

/// The composer. Returns an action when something was sent or attached.
pub fn composer(
    ui: &mut Ui,
    state: &State,
    channel: Id,
    text: &mut String,
    can_send: bool,
) -> Option<Action> {
    let mut action = None;

    // Who is typing, above the field, in the space that is there anyway.
    let typers = state.typers(channel);
    let notice = match typers.len() {
        0 => String::new(),
        1 => format!("{} is typing…", state.label(typers[0])),
        2 => format!("{} and {} are typing…", state.label(typers[0]), state.label(typers[1])),
        n => format!("{n} people are typing…"),
    };
    ui.label(egui::RichText::new(notice).size(10.5).color(theme::TEXT_FAINT));

    ui.horizontal(|ui| {
        let buttons = 76.0;
        let width = (ui.available_width() - buttons).max(80.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 38.0), egui::Sense::hover());
        glass::well(ui, rect, theme::R_CONTROL);

        let mut child =
            ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(10.0, 8.0))));
        let field = child.add(
            egui::TextEdit::multiline(text)
                .frame(egui::Frame::NONE)
                .desired_rows(1)
                .desired_width(f32::INFINITY)
                .hint_text("Message")
                // Enter sends; Shift-Enter is a newline. `return_key` off would make Enter
                // insert a line break, which is the wrong way round for a chat window.
                .lock_focus(false),
        );

        if field.changed() && !text.trim().is_empty() {
            action = Some(Action::Typing(channel));
        }

        let send_pressed = ui.input(|i| {
            i.key_pressed(egui::Key::Enter) && !i.modifiers.shift && !i.modifiers.command
        });
        if field.has_focus() && send_pressed && can_send {
            // The newline that Enter inserted has to come back out: the field has already
            // applied the keystroke by the time this runs.
            if text.ends_with('\n') {
                text.pop();
            }
            if !text.trim().is_empty() {
                action = Some(Action::SendComposer);
            }
        }

        if widgets::icon_button(ui, icons::paperclip, 32.0, "Attach a file").clicked() {
            action = Some(Action::AttachFiles);
        }
        let ready = !text.trim().is_empty() && can_send;
        let colour = if ready { theme::ACCENT } else { theme::TEXT_FAINT };
        if widgets::icon_button_tinted(ui, icons::send, 32.0, "Send", colour, None).clicked() && ready {
            action = Some(Action::SendComposer);
        }
    });

    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grouping_window_is_a_few_minutes() {
        // Pinned because the value is a judgement rather than a fact, and a change to it changes
        // how every conversation reads.
        assert_eq!(GROUP_WINDOW_MS, 300_000);
    }
}
