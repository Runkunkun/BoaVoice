//! Settings: devices, noise handling, screen quality, the attachment store, the server.
//!
//! Grouped by what somebody is trying to fix rather than by which module implements it, which is
//! why "why can nobody hear me" (device, gain, gate, the meter) is one block and reads top to
//! bottom in the order those things go wrong.
//!
//! Two things here are deliberately unlike a commercial client. The screen-sharing block has no
//! upper limits worth the name — resolution, frame rate and bitrate go as far as the hardware
//! does, because the whole point of hosting it yourself is that nobody is selling you the
//! difference. And the attachment block explains the three-day server life in full, with the size
//! of the local store next to it, because that is the one design decision a user has to
//! understand to trust the app with a picture they care about.

use egui::Ui;

use crate::audio::devices::Devices;
use crate::settings::Settings;
use crate::state::State;
use crate::theme;
use crate::ui::{glass, widgets, Action, Meter};

pub fn show(
    ui: &mut Ui,
    settings: &mut Settings,
    state: &State,
    devices: &Devices,
    meter: &Meter,
    images_loaded: usize,
) -> Option<Action> {
    let mut action = None;
    let mut changed = false;

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let width = ui.available_width().min(560.0);
        ui.allocate_ui_with_layout(
            egui::vec2(width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.add_space(8.0);

                // ---------------------------------------------------------------- //
                widgets::section(ui, "Voice — input");
                let slot = glass::reserve(ui);
                let top = ui.cursor().top();
                ui.add_space(10.0);

                changed |= device_picker(
                    ui,
                    "Microphone",
                    &mut settings.voice.input_device,
                    &devices.inputs,
                    width,
                );
                if let Some(chosen) = &settings.voice.input_device {
                    if !devices.has_input(chosen) {
                        // The distinction that matters: this is not "the default", it is "the
                        // thing you chose, which is not here". Both play through the same
                        // hardware and need different explanations.
                        ui.label(
                            egui::RichText::new(format!(
                                "{chosen} is not plugged in — using the system default for now"
                            ))
                            .size(10.5)
                            .color(theme::WARN),
                        );
                    }
                }

                ui.add_space(8.0);
                changed |= slider(ui, "Input gain", &mut settings.voice.input_gain, 0.0..=4.0, |v| {
                    format!("{:.0}%", v * 100.0)
                });

                ui.add_space(6.0);
                changed |= ui
                    .checkbox(&mut settings.voice.noise_suppression, "Noise suppression")
                    .on_hover_text(
                        "Removes steady noise — fans, hiss, traffic — from your voice. \
                         Costs about a millisecond per frame and no bandwidth.",
                    )
                    .changed();

                ui.add_space(6.0);
                // The gate and the meter belong together: the number is meaningless without
                // seeing where your own voice sits relative to it.
                changed |= slider(
                    ui,
                    "Send above",
                    &mut settings.voice.gate_threshold_db,
                    -70.0..=-10.0,
                    |v| format!("{v:.0} dB"),
                );
                let (rect, _) = ui.allocate_exact_size(egui::vec2(width - 24.0, 12.0), egui::Sense::hover());
                widgets::level_meter(ui, rect, meter.input_level, meter.gate_open, meter.threshold);
                ui.label(
                    egui::RichText::new(
                        "The bar is your microphone; the tick is the threshold. \
                         Below it, nothing is transmitted at all.",
                    )
                    .size(10.0)
                    .color(theme::TEXT_FAINT),
                );

                ui.add_space(6.0);
                changed |= ui
                    .checkbox(&mut settings.voice.push_to_talk, "Push to talk")
                    .on_hover_text("Transmit only while a key is held")
                    .changed();
                if settings.voice.push_to_talk {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Key").size(11.5).color(theme::TEXT_DIM));
                        changed |= widgets::field(ui, &mut settings.voice.push_to_talk_key, "F13", 120.0, false).changed();
                    });
                    ui.label(
                        egui::RichText::new(
                            "Named as egui names keys: F13, Space, Backslash. \
                             It only works while this window has focus — a global hotkey needs \
                             permissions this app does not ask for.",
                        )
                        .size(10.0)
                        .color(theme::TEXT_FAINT),
                    );
                }

                ui.add_space(12.0);
                glass::fill_card(ui, slot, card_rect(ui, width, top));

                // ---------------------------------------------------------------- //
                ui.add_space(10.0);
                widgets::section(ui, "Voice — output");
                let slot = glass::reserve(ui);
                let top = ui.cursor().top();
                ui.add_space(10.0);

                changed |= device_picker(
                    ui,
                    "Speakers",
                    &mut settings.voice.output_device,
                    &devices.outputs,
                    width,
                );
                ui.add_space(8.0);
                changed |= slider(ui, "Volume", &mut settings.voice.output_volume, 0.0..=2.0, |v| {
                    format!("{:.0}%", v * 100.0)
                });

                ui.add_space(8.0);
                let mut jitter = settings.voice.jitter_ms as f32;
                if slider(ui, "Buffer", &mut jitter, 20.0..=300.0, |v| format!("{v:.0} ms")) {
                    settings.voice.jitter_ms = jitter as u32;
                    changed = true;
                }
                ui.label(
                    egui::RichText::new(
                        "Every millisecond here is added delay in the conversation, and too few \
                         turns every network hiccup into an audible gap. 60 ms suits wifi; on a \
                         wired LAN 20–40 is noticeably snappier.",
                    )
                    .size(10.0)
                    .color(theme::TEXT_FAINT),
                );

                ui.add_space(12.0);
                glass::fill_card(ui, slot, card_rect(ui, width, top));

                // ---------------------------------------------------------------- //
                ui.add_space(10.0);
                widgets::section(ui, "Screen sharing");
                let slot = glass::reserve(ui);
                let top = ui.cursor().top();
                ui.add_space(10.0);

                let mut edge = settings.screen.max_dimension as f32;
                if slider(ui, "Longest edge", &mut edge, 640.0..=3840.0, |v| format!("{v:.0} px")) {
                    settings.screen.max_dimension = edge as u32;
                    changed = true;
                }
                let mut fps = settings.screen.fps as f32;
                if slider(ui, "Frame rate", &mut fps, 5.0..=144.0, |v| format!("{v:.0} fps")) {
                    settings.screen.fps = fps as u32;
                    changed = true;
                }
                let mut kbps = settings.screen.kbps as f32;
                if slider(ui, "Bitrate", &mut kbps, 500.0..=60_000.0, |v| {
                    if v >= 1_000.0 {
                        format!("{:.1} Mbit/s", v / 1_000.0)
                    } else {
                        format!("{v:.0} kbit/s")
                    }
                }) {
                    settings.screen.kbps = kbps as u32;
                    changed = true;
                }
                changed |= ui
                    .checkbox(&mut settings.screen.with_audio, "Include the desktop's sound")
                    .on_hover_text(
                        "Sent as a second stereo stream at 96 kbit/s, beside the picture",
                    )
                    .changed();
                if settings.screen.with_audio {
                    // Said here rather than only when a share fails: an operating system will not let
                    // a program record its own output without help, and knowing that *before* the
                    // meeting is worth a line of text.
                    let (text, colour) = match crate::screen::find_loopback() {
                        Ok(loopback) => (format!("sound will come from {}", loopback.label), theme::OK),
                        Err(advice) => (advice, theme::WARN),
                    };
                    ui.label(egui::RichText::new(text).size(10.0).color(colour));
                }

                ui.label(
                    egui::RichText::new(
                        "There is no tier here and no server-side cap: these numbers are your \
                         encoder's settings, and the limit is your machine and your uplink. \
                         1080p60 at 8 Mbit/s looks like a local screen; 4K at 40 needs a LAN.",
                    )
                    .size(10.0)
                    .color(theme::ACCENT_DEEP),
                );

                ui.add_space(12.0);
                glass::fill_card(ui, slot, card_rect(ui, width, top));

                // ---------------------------------------------------------------- //
                ui.add_space(10.0);
                widgets::section(ui, "Attachments");
                let slot = glass::reserve(ui);
                let top = ui.cursor().top();
                ui.add_space(10.0);

                let ttl_days = state
                    .server
                    .as_ref()
                    .map(|s| s.attachment_ttl_secs / 86_400)
                    .unwrap_or(boa_proto::ATTACHMENT_TTL_SECS / 86_400);
                ui.label(
                    egui::RichText::new(format!(
                        "The server keeps an attachment's bytes for {ttl_days} days so it does not \
                         fill up. After that, the copy on this computer is the only one — which is \
                         why it is kept next to the settings and not in a cache the system may \
                         empty."
                    ))
                    .size(11.0)
                    .color(theme::TEXT_DIM),
                );

                ui.add_space(8.0);
                let held = crate::cache::total_bytes();
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} here, {} decoded in memory",
                            widgets::bytes(held),
                            images_loaded
                        ))
                        .size(11.5)
                        .color(theme::TEXT),
                    );
                    if widgets::pill_button(ui, "Show the folder", false).clicked() {
                        action = Some(Action::RevealAttachments);
                    }
                });

                ui.add_space(8.0);
                let mut keep_forever = settings.local_retention_days == 0;
                if ui
                    .checkbox(&mut keep_forever, "Keep everything, forever")
                    .on_hover_text("The safe default: after the server's few days, this is the only copy")
                    .changed()
                {
                    settings.local_retention_days = if keep_forever { 0 } else { 90 };
                    changed = true;
                }
                if !keep_forever {
                    let mut days = settings.local_retention_days as f32;
                    if slider(ui, "Delete after", &mut days, 7.0..=730.0, |v| format!("{v:.0} days")) {
                        settings.local_retention_days = days as u32;
                        changed = true;
                    }
                    ui.label(
                        egui::RichText::new(
                            "Anything deleted here cannot be fetched again — the server let go of \
                             it long before.",
                        )
                        .size(10.0)
                        .color(theme::WARN),
                    );
                }

                ui.add_space(12.0);
                glass::fill_card(ui, slot, card_rect(ui, width, top));

                // ---------------------------------------------------------------- //
                ui.add_space(10.0);
                widgets::section(ui, "Account and server");
                let slot = glass::reserve(ui);
                let top = ui.cursor().top();
                ui.add_space(10.0);

                if let Some(me) = &state.me {
                    ui.label(
                        egui::RichText::new(format!("signed in as {}", me.name))
                            .size(11.5)
                            .color(theme::TEXT),
                    );
                }
                if let Some(server) = &state.server {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} · protocol {} · media on UDP {}",
                            server.name, server.protocol_version, server.media_port
                        ))
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                    );
                    match (&server.wormhole_rendezvous, &server.wormhole_transit) {
                        (None, None) => {
                            ui.label(
                                egui::RichText::new(
                                    "direct file transfers use the public wormhole relays — the \
                                     files themselves still go peer to peer and encrypted",
                                )
                                .size(10.5)
                                .color(theme::TEXT_FAINT),
                            );
                        }
                        (rendezvous, transit) => {
                            ui.label(
                                egui::RichText::new(format!(
                                    "file transfers use this server's own relays ({}{})",
                                    rendezvous.clone().unwrap_or_else(|| "default rendezvous".into()),
                                    transit
                                        .as_ref()
                                        .map(|t| format!(", {t}"))
                                        .unwrap_or_default()
                                ))
                                .size(10.5)
                                .color(theme::TEXT_FAINT),
                            );
                        }
                    }
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if widgets::pill_button(ui, "Sign out", false).clicked() {
                        action = Some(Action::LogOut);
                    }
                    if widgets::pill_button(ui, "New voice channel", false).clicked() {
                        action = Some(Action::CreateChannel(
                            "Voice".to_string(),
                            boa_proto::ChannelKind::Voice,
                        ));
                    }
                });

                ui.add_space(10.0);
                if !crate::platform::has_vibrancy() {
                    ui.label(
                        egui::RichText::new(
                            "This platform has no frosted-window service, so the panels are plainly \
                             translucent rather than blurred. Everything else is the same.",
                        )
                        .size(10.0)
                        .color(theme::TEXT_FAINT),
                    );
                }
                ui.label(
                    egui::RichText::new(format!(
                        "last session's log: {}",
                        crate::paths::log_path().display()
                    ))
                    .size(10.0)
                    .color(theme::TEXT_FAINT),
                );

                ui.add_space(12.0);
                glass::fill_card(ui, slot, card_rect(ui, width, top));
                ui.add_space(20.0);
            },
        );
    });

    if changed {
        settings.sanitise();
        action = action.or(Some(Action::SettingsChanged));
    }
    action
}

/// The rectangle a card should cover, from where it started to where the cursor is now.
fn card_rect(ui: &Ui, width: f32, top: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(ui.max_rect().left(), top),
        egui::pos2(ui.max_rect().left() + width, ui.cursor().top()),
    )
}

/// A labelled slider that reports whether it moved.
fn slider(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    format: impl Fn(f32) -> String,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(11.5).color(theme::TEXT_DIM));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(format(*value)).size(11.0).color(theme::TEXT));
            changed = ui
                .add(egui::Slider::new(value, range).show_value(false).trailing_fill(true))
                .changed();
        });
    });
    changed
}

/// A device picker: the system default, then everything else.
///
/// "System default" is a real entry rather than an empty selection, because it is a *choice* —
/// "follow whatever the machine is doing" — and one that most people should make. Representing it
/// as the absence of a choice makes it look like the settings are incomplete.
fn device_picker(
    ui: &mut Ui,
    label: &str,
    chosen: &mut Option<String>,
    available: &[crate::audio::devices::DeviceInfo],
    width: f32,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(11.5).color(theme::TEXT_DIM));
        let shown = chosen.clone().unwrap_or_else(|| "System default".to_string());
        egui::ComboBox::from_id_salt(label)
            .selected_text(shown)
            .width((width - 130.0).max(160.0))
            .show_ui(ui, |ui| {
                if ui.selectable_label(chosen.is_none(), "System default").clicked() {
                    *chosen = None;
                    changed = true;
                }
                for device in available {
                    let selected = chosen.as_deref() == Some(device.name.as_str());
                    let text = if device.is_default {
                        format!("{} (default)", device.name)
                    } else {
                        device.name.clone()
                    };
                    if ui.selectable_label(selected, text).clicked() {
                        *chosen = Some(device.name.clone());
                        changed = true;
                    }
                }
                if available.is_empty() {
                    ui.label(
                        egui::RichText::new("nothing else was found")
                            .size(10.5)
                            .color(theme::TEXT_FAINT),
                    );
                }
            });
    });
    changed
}
