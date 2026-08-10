//! The bar along the bottom: who you are, what is switched off, and how to leave.
//!
//! It is always visible, in and out of a call, and that is the point. The single most common
//! complaint about every voice app is talking while muted, and the fix is not a better dialogue —
//! it is that the state of your microphone is on screen at all times, in the same place, in a
//! colour you do not have to read.
//!
//! When there is no call the same bar carries the connection's state instead, so "reconnecting"
//! is somewhere the eye already goes rather than in a notification that has faded.

use egui::Ui;

use crate::net::Status;
use crate::state::State;
use crate::theme;
use crate::ui::{icons, widgets, Action, Meter};

pub const HEIGHT: f32 = 56.0;

pub fn show(
    ui: &mut Ui,
    state: &State,
    status: &Status,
    meter: &Meter,
    saved: &crate::settings::VoiceSettings,
) -> Option<Action> {
    let mut action = None;

    ui.horizontal_centered(|ui| {
        ui.add_space(10.0);

        // Who we are.
        let (me_label, me_id) = match &state.me {
            Some(me) => (me.label().to_string(), me.id),
            None => ("—".to_string(), boa_proto::Id::NONE),
        };
        let (avatar_rect, _) = ui.allocate_exact_size(egui::Vec2::splat(30.0), egui::Sense::hover());
        let ring = meter.speaking.then_some(theme::SPEAKING);
        widgets::avatar(ui, avatar_rect, &me_label, me_id, ring);

        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(&me_label).size(12.5).color(theme::TEXT));
            ui.label(status_line(state, status, meter));
        });

        // The controls, right-aligned. Laid out right to left so the rightmost — hanging up, the
        // most consequential — is always in the same place whatever else is showing.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(10.0);

            if state.my_channel().is_some()
                && widgets::icon_button_tinted(
                    ui,
                    icons::hang_up,
                    34.0,
                    "Leave the call",
                    theme::ERROR,
                    Some(theme::alpha(theme::ERROR, 34)),
                )
                .clicked()
            {
                action = Some(Action::LeaveVoice);
            }

            if widgets::icon_button(ui, icons::gear, 30.0, "Settings").clicked() {
                action = Some(Action::Go(crate::ui::View::Settings));
            }

            // In a call the server's view is authoritative; outside one, the saved setting is all
            // there is. Either way the buttons are here, because muting before joining is a thing
            // people do on purpose.
            let in_call = state.my_voice().is_some();
            let (muted, deafened) = match state.my_voice() {
                Some(voice) => (voice.muted, voice.deafened),
                None => (saved.muted, saved.deafened),
            };

            if in_call {
                let voice = state.my_voice().expect("just checked");
                let sharing = voice.screen.is_some();
                let (icon, tooltip, colour, fill): (widgets::IconFn, _, _, _) = if sharing {
                    (
                        icons::monitor,
                        "Stop sharing your screen",
                        theme::SHARING,
                        Some(theme::alpha(theme::SHARING, 40)),
                    )
                } else {
                    (icons::monitor_share, "Share your screen", theme::TEXT_DIM, None)
                };
                if widgets::icon_button_tinted(ui, icon, 34.0, tooltip, colour, fill).clicked() {
                    action = Some(if sharing { Action::StopScreen } else { Action::StartScreen });
                }
            }

            {
                // Deafen, then mute, so the pair reads in the order they are usually reached for.
                let (icon, tooltip, colour, fill): (widgets::IconFn, _, _, _) = if deafened {
                    (
                        icons::headphones_off,
                        "Turn the sound back on",
                        theme::DEAFENED,
                        Some(theme::alpha(theme::DEAFENED, 40)),
                    )
                } else {
                    (icons::headphones, "Turn all sound off", theme::TEXT_DIM, None)
                };
                if widgets::icon_button_tinted(ui, icon, 34.0, tooltip, colour, fill).clicked() {
                    action = Some(Action::ToggleDeafen);
                }

                let (icon, tooltip, colour, fill): (widgets::IconFn, _, _, _) = if muted || deafened {
                    (
                        icons::microphone_off,
                        if deafened {
                            "Muted because the sound is off"
                        } else {
                            "Turn the microphone back on"
                        },
                        theme::MUTED,
                        Some(theme::alpha(theme::MUTED, 40)),
                    )
                } else {
                    (icons::microphone, "Mute the microphone", theme::TEXT, None)
                };
                if widgets::icon_button_tinted(ui, icon, 34.0, tooltip, colour, fill).clicked() {
                    action = Some(Action::ToggleMute);
                }

                // The level meter, only in a call and only when the microphone is live: a meter
                // that moves while muted invites exactly the confusion the bar exists to prevent.
                if in_call && !(muted || deafened) {
                    ui.add_space(6.0);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(70.0, 8.0), egui::Sense::hover());
                    widgets::level_meter(ui, rect, meter.input_level, meter.gate_open, meter.threshold);
                }
            }
        });
    });

    action
}

/// The second line under the name: what is happening, in as few words as possible.
fn status_line(state: &State, status: &Status, meter: &Meter) -> egui::RichText {
    // The connection comes first: nothing else on the bar means anything if the socket is down.
    match status {
        Status::Offline => return egui::RichText::new("offline").size(10.5).color(theme::TEXT_FAINT),
        Status::Connecting => {
            return egui::RichText::new("connecting…").size(10.5).color(theme::WARN)
        }
        Status::Reconnecting { in_secs } => {
            return egui::RichText::new(format!("reconnecting in {in_secs}s"))
                .size(10.5)
                .color(theme::WARN)
        }
        Status::Rejected(_) => {
            return egui::RichText::new("signed out").size(10.5).color(theme::ERROR)
        }
        Status::Connected => {}
    }

    match state.my_channel() {
        None => egui::RichText::new(
            state.server.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| "connected".into()),
        )
        .size(10.5)
        .color(theme::TEXT_FAINT),
        Some(channel) => {
            let name = state.channel(channel).map(|c| c.name.clone()).unwrap_or_default();
            // In a call, the useful fact is whether *media* is flowing, which is not the same as
            // whether the control connection is up: the UDP port is separate and is the one that
            // firewalls block. A call where chat works and nobody can hear anybody is the
            // commonest self-hosting mistake, and this line is where it shows.
            if meter.media_ok {
                egui::RichText::new(name).size(10.5).color(theme::OK)
            } else {
                egui::RichText::new(format!("{name} — no voice connection"))
                    .size(10.5)
                    .color(theme::ERROR)
            }
        }
    }
}
