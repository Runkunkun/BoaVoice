//! The Catppuccin Mocha palette, the roles the UI assigns to it, and the egui
//! visuals derived from both.
//!
//! The same three-layer arrangement as its siblings RedPython and BluePython, and for
//! the same reasons. The raw palette is a verbatim copy of the published Mocha
//! flavour, pinned by tests so a typo cannot quietly drift the look. Roles (`GLASS`,
//! `TEXT`, `ACCENT`, …) sit on top: the UI names those, so re-tinting is a change in
//! one file rather than across every widget. Finally [`apply`] pours the roles into
//! egui's own [`egui::Visuals`], because a lot of egui draws itself and needs to be
//! told the palette rather than asked.
//!
//! Mocha rather than a lighter flavour because the window is translucent: the desktop
//! shows through, and only a dark tint keeps text legible over whatever happens to be
//! behind it. Alpha is the other half of the design — nothing in the chrome is fully
//! opaque, so the platform's blur underneath does the actual frosting.
//!
//! Where this one differs from the music players is the small set of roles at the
//! bottom that only a voice app needs: a person can be *speaking*, *muted* or
//! *deafened*, and those three states have to be distinguishable at a glance in a
//! sidebar, in a badge on an avatar, and on the button that toggles them. They are
//! named here rather than picked per widget so the green that means "talking" is the
//! same green everywhere.

use egui::{Color32, CornerRadius, Stroke};

/// Build an opaque colour from a hex literal, e.g. `rgb(0x1e1e2e)`.
const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The same colour at an explicit alpha.
pub const fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(c.r(), c.g(), c.b(), a)
}

/// Mix two colours; `t` runs 0.0…1.0 towards `b`.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()), f(a.a(), b.a()))
}

// --------------------------------------------------------------------------- //
// The raw Mocha palette (catppuccin/palette, flavour "mocha")
// --------------------------------------------------------------------------- //

pub mod mocha {
    use super::{rgb, Color32};

    pub const ROSEWATER: Color32 = rgb(0xf5e0dc);
    pub const FLAMINGO: Color32 = rgb(0xf2cdcd);
    pub const PINK: Color32 = rgb(0xf5c2e7);
    pub const MAUVE: Color32 = rgb(0xcba6f7);
    pub const RED: Color32 = rgb(0xf38ba8);
    pub const MAROON: Color32 = rgb(0xeba0ac);
    pub const PEACH: Color32 = rgb(0xfab387);
    pub const YELLOW: Color32 = rgb(0xf9e2af);
    pub const GREEN: Color32 = rgb(0xa6e3a1);
    pub const TEAL: Color32 = rgb(0x94e2d5);
    pub const SKY: Color32 = rgb(0x89dceb);
    pub const SAPPHIRE: Color32 = rgb(0x74c7ec);
    pub const BLUE: Color32 = rgb(0x89b4fa);
    pub const LAVENDER: Color32 = rgb(0xb4befe);
    pub const TEXT: Color32 = rgb(0xcdd6f4);
    pub const SUBTEXT1: Color32 = rgb(0xbac2de);
    pub const SUBTEXT0: Color32 = rgb(0xa6adc8);
    pub const OVERLAY2: Color32 = rgb(0x9399b2);
    pub const OVERLAY1: Color32 = rgb(0x7f849c);
    pub const OVERLAY0: Color32 = rgb(0x6c7086);
    pub const SURFACE2: Color32 = rgb(0x585b70);
    pub const SURFACE1: Color32 = rgb(0x45475a);
    pub const SURFACE0: Color32 = rgb(0x313244);
    pub const BASE: Color32 = rgb(0x1e1e2e);
    pub const MANTLE: Color32 = rgb(0x181825);
    pub const CRUST: Color32 = rgb(0x11111b);
}

// --------------------------------------------------------------------------- //
// Roles
// --------------------------------------------------------------------------- //

/// The window's own tint, painted under everything.
///
/// Deliberately thin. The frosted look comes from the platform's own blur behind the
/// window; anything heavier here would bury it and leave a flat dark rectangle that
/// merely *looks* like it should be blurred.
pub const WINDOW: Color32 = alpha(mocha::BASE, 178);

/// A raised panel — sidebar, the voice bar, dialogs.
pub const GLASS: Color32 = alpha(mocha::SURFACE0, 132);

/// A panel one step further forward (cards, popovers, message groups).
pub const GLASS_HIGH: Color32 = alpha(mocha::SURFACE1, 150);

/// A recessed well — list backgrounds, text fields, the composer.
pub const GLASS_LOW: Color32 = alpha(mocha::CRUST, 92);

/// The hairline that gives a glass edge its lit rim.
pub const RIM: Color32 = alpha(mocha::TEXT, 26);

/// A stronger rim, for the element holding focus.
pub const RIM_STRONG: Color32 = alpha(mocha::TEXT, 54);

pub const TEXT: Color32 = mocha::TEXT;
pub const TEXT_DIM: Color32 = mocha::SUBTEXT0;
pub const TEXT_FAINT: Color32 = mocha::OVERLAY1;

/// BoaVoice's accent. Green, to match the icon and to separate it at a glance from
/// its red and blue siblings.
pub const ACCENT: Color32 = mocha::GREEN;
pub const ACCENT_DEEP: Color32 = mocha::TEAL;

pub const OK: Color32 = mocha::GREEN;
pub const WARN: Color32 = mocha::YELLOW;
pub const ERROR: Color32 = mocha::RED;

/// Hover and selection washes over a list row.
pub const HOVER: Color32 = alpha(mocha::SURFACE2, 70);
pub const SELECTED: Color32 = alpha(mocha::GREEN, 40);

// --------------------------------------------------------------------------- //
// Voice roles
// --------------------------------------------------------------------------- //

/// The ring around somebody who is talking.
///
/// The same green as the accent, at full strength. Speaking is the one piece of state
/// in the app that changes several times a second, so it has to read instantly and
/// must not be a hue anybody has to compare against a neighbour to identify.
pub const SPEAKING: Color32 = mocha::GREEN;

/// Microphone off. Red, because it is a state the person chose and needs to be
/// reminded of — the commonest confusion in every voice app is talking while muted.
pub const MUTED: Color32 = mocha::RED;

/// Output off. Distinct from [`MUTED`] rather than a second red: being deafened is a
/// different problem from being muted, and an app that shows one colour for both makes
/// "why can nobody hear me" and "why can I not hear anybody" look the same.
pub const DEAFENED: Color32 = mocha::MAROON;

/// Somebody is sharing their screen.
pub const SHARING: Color32 = mocha::MAUVE;

/// A file transfer in flight.
pub const TRANSFER: Color32 = mocha::SAPPHIRE;

/// The level meter's ramp: quiet, present, and clipping.
pub const LEVEL_LOW: Color32 = mocha::TEAL;
pub const LEVEL_MID: Color32 = mocha::GREEN;
pub const LEVEL_HOT: Color32 = mocha::YELLOW;
pub const LEVEL_CLIP: Color32 = mocha::RED;

/// The colour for a given input level, 0.0…1.0.
///
/// A ramp rather than a single colour, because the useful thing to know about your own
/// microphone is not "is it working" but "is it in the right range" — and the top of
/// the ramp has to be alarming, because clipping cannot be fixed after the fact.
pub fn level_colour(level: f32) -> Color32 {
    match level {
        l if l >= 0.95 => LEVEL_CLIP,
        l if l >= 0.75 => mix(LEVEL_MID, LEVEL_HOT, (l - 0.75) / 0.20),
        l if l >= 0.25 => mix(LEVEL_LOW, LEVEL_MID, (l - 0.25) / 0.50),
        _ => LEVEL_LOW,
    }
}

// --------------------------------------------------------------------------- //
// Geometry
// --------------------------------------------------------------------------- //

/// Corner radii, in the same three steps macOS uses: chip, control, panel.
pub const R_CHIP: CornerRadius = CornerRadius::same(6);
pub const R_CONTROL: CornerRadius = CornerRadius::same(10);
pub const R_PANEL: CornerRadius = CornerRadius::same(16);

// --------------------------------------------------------------------------- //
// egui wiring
// --------------------------------------------------------------------------- //

/// Install the palette, spacing and typography on `ctx`.
pub fn apply(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();

    // Every fill is translucent: the window is a single glass stack, and an opaque
    // widget in the middle of it reads as a hole.
    visuals.panel_fill = Color32::TRANSPARENT;
    visuals.window_fill = GLASS;
    visuals.extreme_bg_color = GLASS_LOW;
    visuals.faint_bg_color = alpha(mocha::SURFACE0, 60);
    visuals.code_bg_color = GLASS_LOW;

    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT_DEEP;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = ERROR;

    visuals.selection.bg_fill = SELECTED;
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);

    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = Color32::TRANSPARENT;
    w.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    w.noninteractive.bg_stroke = Stroke::new(1.0, RIM);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    w.noninteractive.corner_radius = R_CONTROL;

    w.inactive.bg_fill = alpha(mocha::SURFACE1, 96);
    w.inactive.weak_bg_fill = alpha(mocha::SURFACE0, 80);
    w.inactive.bg_stroke = Stroke::new(1.0, RIM);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = R_CONTROL;

    w.hovered.bg_fill = alpha(mocha::SURFACE2, 130);
    w.hovered.weak_bg_fill = alpha(mocha::SURFACE1, 110);
    w.hovered.bg_stroke = Stroke::new(1.0, RIM_STRONG);
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.corner_radius = R_CONTROL;
    w.hovered.expansion = 1.0;

    w.active.bg_fill = alpha(ACCENT, 120);
    w.active.weak_bg_fill = alpha(ACCENT, 90);
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(1.0, mocha::CRUST);
    w.active.corner_radius = R_CONTROL;
    w.active.expansion = 0.0;

    w.open.bg_fill = GLASS_HIGH;
    w.open.weak_bg_fill = GLASS;
    w.open.bg_stroke = Stroke::new(1.0, RIM_STRONG);
    w.open.fg_stroke = Stroke::new(1.0, TEXT);
    w.open.corner_radius = R_CONTROL;

    // Shadows would fight the platform blur — a blurred drop shadow over a blurred
    // backdrop just muddies both. The rim stroke separates layers instead. Popups keep
    // a shadow because they float over content rather than sitting in the stack.
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(90),
    };
    visuals.window_shadow = visuals.popup_shadow;
    visuals.window_corner_radius = R_PANEL;
    visuals.window_stroke = Stroke::new(1.0, RIM);
    visuals.menu_corner_radius = R_CONTROL;

    // The app has one look, not a light and a dark one: the whole design rests on
    // light text over a dark frost, and a light variant would need a different palette
    // rather than an inverted one. Pin the preference and give both slots the same
    // visuals, so a system theme switch cannot half-apply.
    ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, visuals.clone());
    ctx.set_visuals_of(egui::Theme::Light, visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.slider_rail_height = 5.0;
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = true;
        // Chat is the exception to its siblings: message text *is* selectable, because
        // copying what somebody said is the second most common thing anybody does in a
        // chat window. The flag is off globally and turned on for the message log.
        style.interaction.selectable_labels = false;
        style.visuals.striped = false;

        use egui::{FontFamily::Proportional, FontId, TextStyle::*};
        style.text_styles = [
            (Heading, FontId::new(21.0, Proportional)),
            (Body, FontId::new(13.5, Proportional)),
            (Button, FontId::new(13.5, Proportional)),
            (Small, FontId::new(11.5, Proportional)),
            (Monospace, FontId::new(12.5, egui::FontFamily::Monospace)),
        ]
        .into();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the palette. These are the published Mocha values; if one of them changes it
    /// should be because the flavour did, not because of a typo.
    #[test]
    fn mocha_values_are_verbatim() {
        assert_eq!(mocha::BASE, Color32::from_rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(mocha::CRUST, Color32::from_rgb(0x11, 0x11, 0x1b));
        assert_eq!(mocha::TEXT, Color32::from_rgb(0xcd, 0xd6, 0xf4));
        assert_eq!(mocha::GREEN, Color32::from_rgb(0xa6, 0xe3, 0xa1));
        assert_eq!(mocha::RED, Color32::from_rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(mocha::MAUVE, Color32::from_rgb(0xcb, 0xa6, 0xf7));
        assert_eq!(mocha::TEAL, Color32::from_rgb(0x94, 0xe2, 0xd5));
    }

    /// The glass roles must stay translucent, or the blur behind the window is wasted
    /// and the app turns into a flat dark rectangle.
    #[test]
    fn glass_roles_are_translucent() {
        for (name, c) in
            [("WINDOW", WINDOW), ("GLASS", GLASS), ("GLASS_HIGH", GLASS_HIGH), ("GLASS_LOW", GLASS_LOW)]
        {
            assert!(c.a() < 255, "{name} must not be opaque (alpha {})", c.a());
            assert!(c.a() > 0, "{name} must not be invisible");
        }
    }

    /// Muted and deafened have to be told apart, or "nobody can hear me" and "I cannot
    /// hear anybody" look like the same problem.
    #[test]
    fn the_voice_states_are_all_distinguishable() {
        let states = [("speaking", SPEAKING), ("muted", MUTED), ("deafened", DEAFENED), ("sharing", SHARING)];
        for (i, (name, a)) in states.iter().enumerate() {
            for (other, b) in &states[i + 1..] {
                let distance = (a.r() as i32 - b.r() as i32).abs()
                    + (a.g() as i32 - b.g() as i32).abs()
                    + (a.b() as i32 - b.b() as i32).abs();
                assert!(distance > 30, "{name} and {other} are too close ({distance})");
            }
        }
    }

    #[test]
    fn the_level_ramp_ends_in_alarm() {
        assert_eq!(level_colour(0.0), LEVEL_LOW);
        assert_eq!(level_colour(1.0), LEVEL_CLIP);
        assert_eq!(level_colour(0.99), LEVEL_CLIP);
        // And is monotone enough not to double back on itself in the middle.
        let quiet = level_colour(0.3);
        let loud = level_colour(0.8);
        assert!(loud.r() > quiet.r(), "the ramp should warm up as it rises");
    }

    #[test]
    fn mix_hits_both_ends() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(255, 255, 255);
        assert_eq!(mix(a, b, 0.0).r(), 0);
        assert_eq!(mix(a, b, 1.0).r(), 255);
        assert_eq!(mix(a, b, 0.5).r(), 128);
        // Out-of-range factors clamp rather than wrap.
        assert_eq!(mix(a, b, 2.0).r(), 255);
        assert_eq!(mix(a, b, -1.0).r(), 0);
    }
}
