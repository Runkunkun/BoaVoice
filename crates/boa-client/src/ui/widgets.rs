//! The controls the app draws that egui does not have.
//!
//! Two kinds of thing live here. Widgets — an icon button, an avatar, a level meter — which
//! exist because egui's own would need the same twenty lines of painting at every call site.
//! And formatting helpers — sizes, relative times, expiry — which exist because they are the
//! sort of code that gets written slightly differently in four places and then disagrees with
//! itself in the one screenshot somebody sends you.

use egui::{Color32, Rect, Response, Sense, Stroke, Ui, Vec2};

use crate::theme;

/// What an icon button draws.
pub type IconFn = fn(&egui::Painter, Rect, Color32);

/// A square button with a vector icon and no label.
///
/// The tooltip is not optional in the signature on purpose: an icon-only control with no
/// tooltip is a guessing game, and making the parameter mandatory means nobody adds one
/// "later".
pub fn icon_button(ui: &mut Ui, icon: IconFn, size: f32, tooltip: &str) -> Response {
    icon_button_tinted(ui, icon, size, tooltip, theme::TEXT_DIM, None)
}

/// An icon button in a chosen colour, optionally with a filled background.
///
/// `fill` is what distinguishes "muted" from "not muted" on the same control: the icon
/// changes *and* the button lights up, because on a busy bar a line through a 16-point glyph
/// is not enough to notice while talking.
pub fn icon_button_tinted(
    ui: &mut Ui,
    icon: IconFn,
    size: f32,
    tooltip: &str,
    colour: Color32,
    fill: Option<Color32>,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let hovered = response.hovered();

    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, theme::R_CONTROL, fill);
    } else if hovered {
        ui.painter().rect_filled(rect, theme::R_CONTROL, theme::HOVER);
    }
    if hovered {
        ui.painter().rect_stroke(
            rect,
            theme::R_CONTROL,
            Stroke::new(1.0, theme::RIM_STRONG),
            egui::StrokeKind::Inside,
        );
    }

    let colour = if hovered { lift(colour) } else { colour };
    icon(ui.painter(), rect.shrink(size * 0.24), colour);

    response.on_hover_text(tooltip)
}

/// A pill-shaped button with a label, for the connect screen and dialogs.
pub fn pill_button(ui: &mut Ui, label: &str, accent: bool) -> Response {
    let text: egui::WidgetText = label.into();
    let galley = text.into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Button);
    let padding = egui::vec2(18.0, 9.0);
    let (rect, response) =
        ui.allocate_exact_size(galley.size() + padding * 2.0, Sense::click());

    let (fill, stroke_colour, text_colour) = match (accent, response.hovered()) {
        (true, false) => (theme::alpha(theme::ACCENT, 40), theme::ACCENT, theme::ACCENT),
        (true, true) => (theme::alpha(theme::ACCENT, 70), theme::ACCENT, theme::TEXT),
        (false, false) => (theme::GLASS_HIGH, theme::RIM, theme::TEXT_DIM),
        (false, true) => (theme::alpha(theme::mocha::SURFACE2, 140), theme::RIM_STRONG, theme::TEXT),
    };
    ui.painter().rect_filled(rect, theme::R_CONTROL, fill);
    ui.painter().rect_stroke(
        rect,
        theme::R_CONTROL,
        Stroke::new(1.0, stroke_colour),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, text_colour);
    response
}

/// A round avatar: initials on a colour derived from the name.
///
/// Derived rather than random or stored, so the same person is the same colour in every
/// client and across restarts without anything having to be transmitted. The hue comes from
/// the id, which is stable even when somebody renames themselves.
pub fn avatar(ui: &mut Ui, rect: Rect, label: &str, id: boa_proto::Id, ring: Option<Color32>) {
    let radius = rect.width().min(rect.height()) / 2.0;
    let centre = rect.center();
    let base = avatar_colour(id);
    ui.painter().circle_filled(centre, radius, theme::alpha(base, 200));

    // Initials, up to two, from the first letters of the first two words.
    let initials: String = label
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .flat_map(|c| c.to_uppercase())
        .collect();
    let initials = if initials.is_empty() { "?".to_string() } else { initials };
    ui.painter().text(
        centre,
        egui::Align2::CENTER_CENTER,
        initials,
        egui::FontId::proportional(radius * 0.85),
        theme::mocha::CRUST,
    );

    if let Some(ring) = ring {
        // Outside the circle rather than on it, so the ring appearing does not appear to
        // shrink the avatar.
        ui.painter().circle_stroke(centre, radius + 1.5, Stroke::new(2.0, ring));
    }
}

/// A stable colour per user id.
///
/// Nine hues from the palette, picked by a cheap hash of the id. Palette colours rather than
/// an HSL sweep because the whole app is Catppuccin and an arbitrary hue looks like it wandered
/// in from another program.
pub fn avatar_colour(id: boa_proto::Id) -> Color32 {
    const HUES: [Color32; 9] = [
        theme::mocha::BLUE,
        theme::mocha::GREEN,
        theme::mocha::PEACH,
        theme::mocha::MAUVE,
        theme::mocha::TEAL,
        theme::mocha::YELLOW,
        theme::mocha::PINK,
        theme::mocha::SAPPHIRE,
        theme::mocha::LAVENDER,
    ];
    // Multiply before taking the remainder: consecutive ids would otherwise get consecutive
    // hues, and the first nine people to join would appear in palette order, which looks
    // deliberate in a way that misleads.
    HUES[(id.0.wrapping_mul(2_654_435_761) % HUES.len() as u64) as usize]
}

/// A horizontal input-level meter.
///
/// `level` is 0…1 peak, `gate_open` says whether that level is currently being transmitted.
/// Both are drawn, because the useful question is not "is my microphone picking anything up"
/// but "is what it picks up getting through" — and a meter that moves while the gate is shut
/// is exactly the confusing case this makes visible.
pub fn level_meter(ui: &mut Ui, rect: Rect, level: f32, gate_open: bool, threshold: f32) {
    super::glass::well(ui, rect, theme::R_CHIP);
    let level = level.clamp(0.0, 1.0);

    let inner = rect.shrink2(egui::vec2(2.0, 3.0));
    if level > 0.001 {
        let filled = Rect::from_min_size(inner.min, egui::vec2(inner.width() * level, inner.height()));
        let colour = theme::level_colour(level);
        // Dimmed while the gate is shut: the signal is there and is not being sent, and those
        // are different facts that a single bar cannot say.
        let colour = if gate_open { colour } else { theme::alpha(colour, 90) };
        ui.painter().rect_filled(filled, theme::R_CHIP, colour);
    }

    // The threshold, as a tick. Dragging it is the settings screen's job; here it is only
    // shown, so that somebody can see how far their voice is above it.
    let x = inner.left() + inner.width() * threshold.clamp(0.0, 1.0);
    ui.painter().line_segment(
        [egui::pos2(x, rect.top() + 1.0), egui::pos2(x, rect.bottom() - 1.0)],
        Stroke::new(1.0, theme::alpha(theme::TEXT, 120)),
    );
}

/// A section heading inside a panel.
pub fn section(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(10.5)
            .color(theme::TEXT_FAINT),
    );
    ui.add_space(2.0);
}

/// A one-line text field on a recessed well.
pub fn field(ui: &mut Ui, text: &mut String, hint: &str, width: f32, password: bool) -> Response {
    let height = 32.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
    super::glass::well(ui, rect, theme::R_CONTROL);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(8.0, 6.0))));
    child.add(
        egui::TextEdit::singleline(text)
            .frame(egui::Frame::NONE)
            .password(password)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    )
}

/// Brighten a colour for a hover state, without leaving the palette's register.
fn lift(colour: Color32) -> Color32 {
    theme::mix(colour, theme::TEXT, 0.45)
}

// --------------------------------------------------------------------------- //
// Formatting
// --------------------------------------------------------------------------- //

/// A byte count, as a person would say it.
///
/// Binary steps with decimal names, which is what every file manager does and what people
/// therefore expect — the pedantically correct "kiB" reads as a typo.
pub fn bytes(count: u64) -> String {
    const KIB: f64 = 1024.0;
    let count = count as f64;
    if count < KIB {
        return format!("{count:.0} B");
    }
    // Divisor and suffix together, so the two cannot get out of step — which is exactly what
    // went wrong in the version that derived one from the other.
    let (scaled, suffix) = if count < KIB * KIB {
        (count / KIB, "kB")
    } else if count < KIB * KIB * KIB {
        (count / (KIB * KIB), "MB")
    } else {
        (count / (KIB * KIB * KIB), "GB")
    };
    // One decimal below ten, none above: "1.4 MB" and "230 MB", never "1 MB" for anything
    // between one and two.
    if scaled < 10.0 {
        format!("{scaled:.1} {suffix}")
    } else {
        format!("{scaled:.0} {suffix}")
    }
}

/// A clock time for a message, or a date for an older one.
///
/// `now` is passed in rather than read, so the tests are not about what time it is.
pub fn message_time(created_at: boa_proto::Millis, now: boa_proto::Millis) -> String {
    let (hour, minute) = clock(created_at);
    let day = created_at.div_euclid(86_400_000);
    let today = now.div_euclid(86_400_000);
    match today - day {
        0 => format!("{hour:02}:{minute:02}"),
        1 => format!("yesterday {hour:02}:{minute:02}"),
        // Beyond a day, the time alone is ambiguous and "3 days ago" is worse than a date
        // when scrolling through a long history.
        _ => {
            let (year, month, day) = civil_date(day);
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
        }
    }
}

/// How long until an attachment leaves the server, in words.
///
/// The one place the three-day policy is stated to the user, so it says what happens next as
/// well as when: after this, the copy on this machine is the only one.
pub fn expiry(expires_at: boa_proto::Millis, now: boa_proto::Millis, have_local: bool) -> String {
    let left = expires_at - now;
    if left <= 0 {
        return if have_local {
            "kept on this computer only".to_string()
        } else {
            "no longer on the server".to_string()
        };
    }
    let hours = left / 3_600_000;
    // Hours up to three days, because three days is the whole life of an attachment here and
    // "2 days" is a vaguer answer than "70 h" to the only question being asked: is there time
    // to come back for this later.
    let when = match hours {
        0 => format!("{} min", (left / 60_000).max(1)),
        1..=72 => format!("{hours} h"),
        _ => format!("{} days", hours / 24),
    };
    if have_local {
        format!("on the server for {when} — already saved here")
    } else {
        format!("on the server for {when}")
    }
}

/// Hours and minutes, UTC.
///
/// UTC and not local time, and this is a real limitation rather than a decision: the client
/// has no timezone database and `chrono` for one label is a large dependency. It is written
/// down here so it is a known gap rather than a mystery about why timestamps look wrong in
/// the afternoon.
fn clock(millis: boa_proto::Millis) -> (i64, i64) {
    let seconds = millis.div_euclid(1000);
    let day_seconds = seconds.rem_euclid(86_400);
    (day_seconds / 3_600, (day_seconds % 3_600) / 60)
}

/// Turn a day number since the epoch into a civil date.
///
/// Howard Hinnant's `civil_from_days`, which is the standard way to do this without a
/// calendar library: shift the era so leap days land at the end, then walk out year, day of
/// year and month with integer arithmetic.
fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boa_proto::Id;

    #[test]
    fn byte_counts_read_the_way_people_say_them() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1024), "1.0 kB");
        assert_eq!(bytes(1536), "1.5 kB");
        assert_eq!(bytes(20 * 1024), "20 kB");
        assert_eq!(bytes(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
        assert_eq!(bytes(230 * 1024 * 1024), "230 MB");
        assert_eq!(bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn a_messages_time_becomes_a_date_once_it_is_old() {
        let day = 86_400_000i64;
        // 2026-08-10T14:35Z, and the same instant seen from later days.
        let at = 20_675 * day + 14 * 3_600_000 + 35 * 60_000;
        assert_eq!(message_time(at, at + 60_000), "14:35");
        assert_eq!(message_time(at, at + day), "yesterday 14:35");
        assert!(message_time(at, at + 5 * day).starts_with("2026-08-10"), "{}", message_time(at, at + 5 * day));
    }

    #[test]
    fn the_civil_date_matches_known_days() {
        // Day zero is 1970-01-01, and a few dates either side of a leap day.
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(-1), (1969, 12, 31));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        assert_eq!(civil_date(20_675), (2026, 8, 10));
    }

    /// The wording is the user-facing statement of the storage policy, so it is pinned.
    #[test]
    fn expiry_says_what_happens_next_not_just_when() {
        let hour = 3_600_000i64;
        let now = 1_000_000_000i64;

        assert_eq!(expiry(now + 70 * hour, now, false), "on the server for 70 h");
        assert!(expiry(now + 70 * hour, now, true).contains("already saved here"));
        assert_eq!(expiry(now + 30 * 60_000, now, false), "on the server for 30 min");
        assert_eq!(expiry(now + 100 * hour, now, false), "on the server for 4 days");

        // Past its time, the sentence has to distinguish "you have it" from "nobody does".
        assert_eq!(expiry(now - hour, now, true), "kept on this computer only");
        assert_eq!(expiry(now - hour, now, false), "no longer on the server");
        // And never round down to zero minutes while it is still alive.
        assert_eq!(expiry(now + 1_000, now, false), "on the server for 1 min");
    }

    #[test]
    fn avatar_colours_are_stable_and_not_in_palette_order() {
        assert_eq!(avatar_colour(Id(7)), avatar_colour(Id(7)));
        // Consecutive ids must not walk the palette in order, which would look like a
        // pattern that means something.
        let first: Vec<Color32> = (1..=4).map(|id| avatar_colour(Id(id))).collect();
        let second: Vec<Color32> = (2..=5).map(|id| avatar_colour(Id(id))).collect();
        assert_ne!(first, second);
    }
}
