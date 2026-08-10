//! The left column: channels, who is in the voice ones, and everybody else.
//!
//! One list rather than the two-panel arrangement Discord uses (channels on the left, members
//! on the right). The reason is that in a self-hosted server for a handful of people the member
//! list is short and the interesting thing about a person is *which call they are in*, which a
//! separate panel cannot show without repeating the whole channel list inside itself. So voice
//! channels list their occupants inline, and the people who are not in a call are listed once at
//! the bottom.

use egui::Ui;

use boa_proto::{ChannelKind, Id};

use crate::state::State;
use crate::theme;
use crate::ui::{glass, icons, widgets, Action};

/// The sidebar's width. Fixed rather than resizable: everything in it is a short name, and a
/// draggable splitter is a control people move once by accident and then have to move back.
pub const WIDTH: f32 = 232.0;

pub fn show(ui: &mut Ui, state: &State, open: Option<Id>, new_channel: &mut String) -> Option<Action> {
    let mut action = None;

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(4.0);

        // Text channels.
        widgets::section(ui, "Channels");
        for channel in state.channels.iter().filter(|c| c.kind == ChannelKind::Text) {
            if channel_row(ui, &channel.name, icons::hash, open == Some(channel.id), false).clicked() {
                action = Some(Action::OpenChannel(channel.id));
            }
        }

        ui.add_space(10.0);
        widgets::section(ui, "Voice");
        for channel in state.channels.iter().filter(|c| c.kind == ChannelKind::Voice) {
            let members = state.voice_members(channel.id);
            let joined = state.my_channel() == Some(channel.id);
            let row = channel_row(ui, &channel.name, icons::speaker, open == Some(channel.id), joined);
            if row.clicked() {
                // One click does both: opens the channel's little chat *and* joins the call if
                // we are not in it. Joining is what somebody clicking a voice channel means;
                // needing a second click on a "join" button is a step nobody wants.
                action = Some(if joined {
                    Action::OpenChannel(channel.id)
                } else {
                    Action::JoinVoice(channel.id)
                });
            }

            for member in &members {
                let response = voice_member_row(ui, state, member);
                if response.clicked() && member.screen.is_some() {
                    action = Some(Action::Watch(member.user));
                }
                if let Some(chosen) = person_menu(response, state, member.user) {
                    action = Some(chosen);
                }
            }
            if !members.is_empty() {
                ui.add_space(4.0);
            }
        }

        // Everybody who is not in a call. Only the online ones by default — a list of everybody
        // who has ever had an account is a list nobody reads.
        ui.add_space(10.0);
        let idle: Vec<_> = state
            .users
            .values()
            .filter(|user| user.online && !state.voice.contains_key(&user.id))
            .collect();
        if !idle.is_empty() {
            widgets::section(ui, "Online");
            for user in idle {
                let response = person_row(ui, user.label(), user.id, true);
                if let Some(chosen) = person_menu(response, state, user.id) {
                    action = Some(chosen);
                }
            }
        }
        let offline: Vec<_> = state.users.values().filter(|user| !user.online).collect();
        if !offline.is_empty() {
            ui.add_space(8.0);
            widgets::section(ui, "Offline");
            for user in offline {
                person_row(ui, user.label(), user.id, false);
            }
        }

        ui.add_space(12.0);
        glass::divider(ui);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let width = ui.available_width() - 34.0;
            let typed = widgets::field(ui, new_channel, "new channel", width.max(60.0), false);
            let submitted = typed.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = widgets::icon_button(ui, icons::hash, 26.0, "Create a text channel").clicked();
            // A voice channel is created by prefixing the name with a speaker's name — no. It is
            // created by the settings screen; here, plain Enter makes a text channel, which is
            // what a text field in a channel list means.
            if (submitted || clicked) && !new_channel.trim().is_empty() {
                action = Some(Action::CreateChannel(new_channel.clone(), ChannelKind::Text));
                new_channel.clear();
            }
        });
        ui.add_space(8.0);
    });

    action
}

/// A channel row: an icon, a name, and a dot when we are in it.
fn channel_row(ui: &mut Ui, name: &str, icon: widgets::IconFn, open: bool, joined: bool) -> egui::Response {
    let height = 26.0;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
    glass::row_highlight(ui, rect, open, response.hovered());

    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 13.0, rect.center().y),
        egui::Vec2::splat(15.0),
    );
    let colour = if open { theme::TEXT } else { theme::TEXT_DIM };
    icon(ui.painter(), icon_rect, colour);

    ui.painter().text(
        egui::pos2(rect.left() + 27.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(13.0),
        colour,
    );

    if joined {
        ui.painter().circle_filled(egui::pos2(rect.right() - 10.0, rect.center().y), 3.5, theme::ACCENT);
    }
    response
}

/// Somebody in a voice channel: avatar, name, and what they have switched off.
fn voice_member_row(ui: &mut Ui, state: &State, member: &boa_proto::VoiceState) -> egui::Response {
    let height = 24.0;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
    if response.hovered() {
        glass::row_highlight(ui, rect, false, true);
    }

    let label = state.label(member.user);
    // The ring is the speaking indicator, and it is on the avatar rather than beside it so that
    // it is visible at a glance down the column without reading anything.
    let ring = state.is_speaking(member.user).then_some(theme::SPEAKING);
    let avatar_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 30.0, rect.center().y),
        egui::Vec2::splat(16.0),
    );
    widgets::avatar(ui, avatar_rect, &label, member.user, ring);

    let colour = if state.is_speaking(member.user) { theme::TEXT } else { theme::TEXT_DIM };
    ui.painter().text(
        egui::pos2(rect.left() + 44.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &label,
        egui::FontId::proportional(12.0),
        colour,
    );

    // Badges, right to left, in the order of how much they matter to somebody looking.
    let mut x = rect.right() - 12.0;
    let badge = egui::Vec2::splat(13.0);
    if member.screen.is_some() {
        let at = egui::Rect::from_center_size(egui::pos2(x, rect.center().y), badge);
        icons::monitor(ui.painter(), at, theme::SHARING);
        x -= 17.0;
    }
    if member.deafened {
        let at = egui::Rect::from_center_size(egui::pos2(x, rect.center().y), badge);
        icons::headphones_off(ui.painter(), at, theme::DEAFENED);
        x -= 17.0;
    }
    // Deafened implies muted on the wire, so showing both badges would be noise; the mute badge
    // appears only when muting is the whole story.
    if member.muted && !member.deafened {
        let at = egui::Rect::from_center_size(egui::pos2(x, rect.center().y), badge);
        icons::microphone_off(ui.painter(), at, theme::MUTED);
    }

    if member.screen.is_some() {
        response.on_hover_text(format!("{label} is sharing a screen — click to watch"))
    } else {
        response
    }
}

/// What you can do with a person: a context menu, so the row's own click keeps its meaning.
///
/// A right-click menu rather than a button on every row. The list is a list of *people*, and hanging
/// an icon off each one for something used occasionally would make the common case — reading who is
/// here — busier for the sake of the rare one.
fn person_menu(response: egui::Response, state: &State, user: Id) -> Option<Action> {
    // Never on yourself: a wormhole to your own machine is not a thing anybody wants.
    if state.me.as_ref().is_some_and(|me| me.id == user) {
        return None;
    }
    let mut action = None;
    response.context_menu(|ui| {
        if ui.button("Send a file directly…").clicked() {
            action = Some(Action::SendFileDirect(user));
            ui.close();
        }
        if state.voice.get(&user).and_then(|state| state.screen).is_some()
            && ui.button("Watch their screen").clicked()
        {
            action = Some(Action::Watch(user));
            ui.close();
        }
    });
    action
}

fn person_row(ui: &mut Ui, label: &str, id: Id, online: bool) -> egui::Response {
    let height = 24.0;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
    if response.hovered() {
        glass::row_highlight(ui, rect, false, true);
    }

    let avatar_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        egui::Vec2::splat(16.0),
    );
    widgets::avatar(ui, avatar_rect, label, id, None);
    if !online {
        // Dimmed by painting the panel's own tint back over it, rather than by drawing the
        // avatar in a different colour — which would lose the per-person hue that makes the
        // list scannable.
        ui.painter().circle_filled(avatar_rect.center(), 8.0, theme::alpha(theme::mocha::BASE, 150));
    }

    ui.painter().text(
        egui::pos2(rect.left() + 30.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.0),
        if online { theme::TEXT_DIM } else { theme::TEXT_FAINT },
    );
    response
}
