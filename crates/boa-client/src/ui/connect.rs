//! The connect screen: which server, and who you are there.
//!
//! It is the first thing anybody sees and the only screen where the app has no idea what is
//! going on, so it is built around saying so. A server is *probed* before any credentials are
//! asked for, which turns the three failures that all look identical from a login form — wrong
//! address, server not running, incompatible version — into three different sentences before
//! anybody has typed a password.
//!
//! Registration is offered only when the server said it would accept one. A self-hosted box
//! whose owner closed registration should not present a form that is guaranteed to be refused.

use egui::Ui;

use crate::net::api::ServerProbe;
use crate::theme;
use crate::ui::{glass, widgets, Action};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    LogIn,
    Register,
}

/// Everything typed on this screen.
#[derive(Default)]
pub struct Form {
    pub url: String,
    pub name: String,
    pub password: String,
    pub display_name: String,
    pub mode: Option<Mode>,
    /// What the server said about itself.
    pub probe: Option<ServerProbe>,
    /// A request is in flight; the buttons are disabled and say what is happening.
    pub busy: Option<&'static str>,
    /// The last failure, shown under the form.
    pub error: Option<String>,
}

impl Form {
    /// Whether the form is ready to be submitted.
    fn complete(&self) -> bool {
        let credentials = !self.name.trim().is_empty() && !self.password.is_empty();
        match self.mode {
            None => false,
            Some(Mode::LogIn) => credentials,
            // The client checks the length here as well as the server, because a rejection
            // after a round trip for something visible before it is a round trip nobody
            // needed.
            Some(Mode::Register) => credentials && self.password.chars().count() >= 12,
        }
    }
}

pub fn show(ui: &mut Ui, form: &mut Form, logo: Option<&egui::TextureHandle>) -> Option<Action> {
    let mut action = None;

    // A fixed-width card, centred. The window can be any size; a login form that stretches to
    // 1400 points wide is a login form with a text field you cannot find the end of.
    let width = 380.0_f32.min(ui.available_width() - 32.0);
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.12).min(90.0));

        let slot = glass::reserve(ui);
        let top = ui.cursor().top();

        ui.allocate_ui_with_layout(
            egui::vec2(width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(30.0), egui::Sense::hover());
                    if let Some(logo) = logo {
                        ui.painter().image(
                            logo.id(),
                            rect,
                            crate::ui::FULL_TEXTURE,
                            egui::Color32::WHITE,
                        );
                    }
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("BoaVoice").size(19.0).color(theme::TEXT).strong());
                });
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.label(
                        egui::RichText::new("Your own server. Voice, screens and files.")
                            .size(11.5)
                            .color(theme::TEXT_FAINT),
                    );
                });
                ui.add_space(14.0);

                let inner = width - 40.0;
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.vertical(|ui| {
                        widgets::section(ui, "Server");
                        let typed = widgets::field(ui, &mut form.url, "boa.example.com:8787", inner, false);
                        if typed.changed() {
                            // The probe belonged to the previous address, and showing a green
                            // server name next to a half-edited URL is worse than showing
                            // nothing.
                            form.probe = None;
                            form.mode = None;
                            form.error = None;
                        }
                        let submitted =
                            typed.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                        ui.add_space(8.0);
                        match (&form.probe, &form.busy) {
                            (Some(probe), _) => server_summary(ui, probe),
                            (None, Some(what)) => {
                                ui.label(egui::RichText::new(*what).size(11.5).color(theme::TEXT_DIM));
                            }
                            (None, None) => {
                                if widgets::pill_button(ui, "Look up", true).clicked() || submitted {
                                    action = Some(Action::Probe);
                                }
                            }
                        }

                        if let Some(probe) = form.probe.clone() {
                            ui.add_space(10.0);
                            glass::divider(ui);
                            if let Some(chosen) = credentials(ui, form, &probe, inner) {
                                action = Some(chosen);
                            }
                        }

                        if let Some(error) = &form.error {
                            ui.add_space(10.0);
                            ui.label(egui::RichText::new(error).size(11.5).color(theme::ERROR));
                        }
                    });
                });
                ui.add_space(20.0);
            },
        );

        let card = egui::Rect::from_min_max(
            egui::pos2(ui.max_rect().center().x - width / 2.0, top),
            egui::pos2(ui.max_rect().center().x + width / 2.0, ui.cursor().top()),
        );
        glass::fill_card(ui, slot, card);
    });

    action
}

/// What the server told us about itself, in one line each.
fn server_summary(ui: &mut Ui, probe: &ServerProbe) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&probe.name).size(14.0).color(theme::ACCENT).strong());
        ui.label(
            egui::RichText::new(format!("protocol {}", probe.protocol_version))
                .size(11.0)
                .color(theme::TEXT_FAINT),
        );
    });
    // The two facts about this server that change how somebody uses it, said before they
    // commit to it rather than discovered later.
    ui.label(
        egui::RichText::new(format!(
            "attachments stay on the server for {} days, then only on your own machine",
            probe.attachment_ttl_secs / 86_400
        ))
        .size(11.0)
        .color(theme::TEXT_DIM),
    );
    ui.label(
        egui::RichText::new(format!(
            "voice and screens on UDP {} — that port has to be open too",
            probe.media_port
        ))
        .size(11.0)
        .color(theme::TEXT_DIM),
    );
}

fn credentials(ui: &mut Ui, form: &mut Form, probe: &ServerProbe, width: f32) -> Option<Action> {
    let mut action = None;

    if form.mode.is_none() {
        // Default to logging in; registration is the rarer path and, on a closed server, not
        // a path at all.
        form.mode = Some(Mode::LogIn);
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let mut mode = form.mode.unwrap_or(Mode::LogIn);
        if ui.selectable_value(&mut mode, Mode::LogIn, "Log in").clicked() {
            form.error = None;
        }
        if probe.registration_open
            && ui.selectable_value(&mut mode, Mode::Register, "New account").clicked()
        {
            form.error = None;
        }
        form.mode = Some(mode);
    });
    if !probe.registration_open {
        ui.label(
            egui::RichText::new("this server is not taking new accounts")
                .size(10.5)
                .color(theme::TEXT_FAINT),
        );
    }

    ui.add_space(8.0);
    widgets::section(ui, "Name");
    widgets::field(ui, &mut form.name, "ada", width, false);

    if form.mode == Some(Mode::Register) {
        ui.add_space(6.0);
        widgets::section(ui, "Shown to others");
        widgets::field(ui, &mut form.display_name, "Ada L. (optional)", width, false);
    }

    ui.add_space(6.0);
    widgets::section(ui, "Password");
    let password = widgets::field(ui, &mut form.password, "", width, true);
    if form.mode == Some(Mode::Register) {
        let typed = form.password.chars().count();
        let (text, colour) = if typed == 0 {
            ("at least 12 characters — length, not punctuation".to_string(), theme::TEXT_FAINT)
        } else if typed < 12 {
            (format!("{} more to go", 12 - typed), theme::WARN)
        } else {
            ("long enough".to_string(), theme::OK)
        };
        ui.label(egui::RichText::new(text).size(10.5).color(colour));
    }

    let submitted = password.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        match form.busy {
            Some(what) => {
                ui.spinner();
                ui.label(egui::RichText::new(what).size(11.5).color(theme::TEXT_DIM));
            }
            None => {
                let label = match form.mode {
                    Some(Mode::Register) => "Create account",
                    _ => "Log in",
                };
                let ready = form.complete();
                // Drawn disabled rather than hidden: a button that appears when the form
                // happens to be valid is a button people do not find.
                let response = ui
                    .add_enabled_ui(ready, |ui| widgets::pill_button(ui, label, true))
                    .inner;
                if (response.clicked() || (submitted && ready)) && ready {
                    action = Some(match form.mode {
                        Some(Mode::Register) => Action::Register,
                        _ => Action::LogIn,
                    });
                }
            }
        }
    });

    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_is_only_submittable_once_it_could_succeed() {
        let mut form = Form::default();
        assert!(!form.complete(), "no server has been chosen yet");

        form.mode = Some(Mode::LogIn);
        assert!(!form.complete());
        form.name = "ada".into();
        assert!(!form.complete());
        form.password = "short".into();
        assert!(form.complete(), "logging in with an old short password must be possible");

        // Registering is held to the server's rule, so the refusal happens before the round
        // trip rather than after it.
        form.mode = Some(Mode::Register);
        assert!(!form.complete());
        form.password = "twelve chars".into();
        assert!(form.complete());

        // Whitespace is not a name.
        form.name = "   ".into();
        assert!(!form.complete());
    }
}
