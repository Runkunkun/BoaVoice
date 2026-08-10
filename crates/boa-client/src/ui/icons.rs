//! The app's icons, drawn as vectors.
//!
//! No icon font and no PNGs. A font would be a file to ship, licence and keep in step with
//! three package formats, and would still be the wrong size on a fractional-scale display;
//! a bitmap has to exist at every scale factor anybody might use. These are a dozen shapes
//! made of lines and arcs, so drawing them is a handful of `painter` calls that are crisp at
//! any size and pick up the palette for free.
//!
//! Every function takes the *box* to draw in and fits itself inside, so a caller lays out
//! rectangles and never has to know an icon's internal proportions. Stroke width comes from
//! the box rather than being a parameter: an icon drawn at 12 points with a 2-point stroke is
//! a blob, and asking each call site to work that out is asking for a different answer at
//! each one.

use egui::{Color32, Pos2, Rect, Shape, Stroke, Vec2};

/// A stroke that suits an icon this size.
///
/// Just under a tenth of the box, clamped so a very small icon still has a visible line and
/// a very large one does not turn into a logo.
fn stroke(rect: Rect, colour: Color32) -> Stroke {
    let width = (rect.width().min(rect.height()) * 0.095).clamp(1.0, 2.5);
    Stroke::new(width, colour)
}

/// The square an icon actually draws in: the largest centred square, inset a little so a
/// stroke cannot land outside the box a caller reserved.
fn field(rect: Rect) -> Rect {
    let side = rect.width().min(rect.height());
    Rect::from_center_size(rect.center(), Vec2::splat(side)).shrink(side * 0.08)
}

/// A microphone: a capsule, its cradle, and a stand.
pub fn microphone(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    let capsule_width = f.width() * 0.34;
    let capsule = Rect::from_center_size(
        egui::pos2(f.center().x, f.top() + f.height() * 0.30),
        egui::vec2(capsule_width, f.height() * 0.48),
    );
    painter.rect_stroke(capsule, egui::CornerRadius::same(
        (capsule_width / 2.0).round() as u8,
    ), s, egui::StrokeKind::Middle);

    // The cradle, as an arc under the capsule.
    let cradle = Rect::from_center_size(
        egui::pos2(f.center().x, f.top() + f.height() * 0.55),
        egui::vec2(f.width() * 0.64, f.height() * 0.42),
    );
    painter.add(arc(cradle, std::f32::consts::PI * 0.05, std::f32::consts::PI * 0.95, s));

    // The stand.
    let bottom = f.bottom() - f.height() * 0.04;
    painter.line_segment(
        [egui::pos2(f.center().x, cradle.bottom()), egui::pos2(f.center().x, bottom)],
        s,
    );
    painter.line_segment(
        [egui::pos2(f.center().x - f.width() * 0.20, bottom), egui::pos2(f.center().x + f.width() * 0.20, bottom)],
        s,
    );
}

/// A microphone with a line through it.
///
/// A struck-through icon rather than a different shape, because the two states have to be
/// recognisable as *the same control* in two positions — and because a slash is the one
/// convention everybody already reads as "off".
pub fn microphone_off(painter: &egui::Painter, rect: Rect, colour: Color32) {
    microphone(painter, rect, colour);
    slash(painter, rect, colour);
}

/// Headphones: a band and two cups.
pub fn headphones(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    let band = Rect::from_center_size(
        egui::pos2(f.center().x, f.top() + f.height() * 0.52),
        egui::vec2(f.width() * 0.86, f.height() * 0.86),
    );
    painter.add(arc(band, std::f32::consts::PI, std::f32::consts::TAU, s));

    let cup = egui::vec2(f.width() * 0.20, f.height() * 0.34);
    let y = f.top() + f.height() * 0.62;
    for x in [band.left(), band.right()] {
        painter.rect_stroke(
            Rect::from_center_size(egui::pos2(x, y), cup),
            egui::CornerRadius::same((cup.x / 2.0).round() as u8),
            s,
            egui::StrokeKind::Middle,
        );
    }
}

pub fn headphones_off(painter: &egui::Painter, rect: Rect, colour: Color32) {
    headphones(painter, rect, colour);
    slash(painter, rect, colour);
}

/// A display: a screen and a foot.
pub fn monitor(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    let screen = Rect::from_min_max(
        egui::pos2(f.left(), f.top() + f.height() * 0.10),
        egui::pos2(f.right(), f.top() + f.height() * 0.70),
    );
    painter.rect_stroke(screen, egui::CornerRadius::same(2), s, egui::StrokeKind::Middle);
    painter.line_segment(
        [egui::pos2(f.center().x, screen.bottom()), egui::pos2(f.center().x, f.bottom() - f.height() * 0.10)],
        s,
    );
    painter.line_segment(
        [
            egui::pos2(f.center().x - f.width() * 0.22, f.bottom() - f.height() * 0.10),
            egui::pos2(f.center().x + f.width() * 0.22, f.bottom() - f.height() * 0.10),
        ],
        s,
    );
}

/// A display with an arrow leaving it: start sharing.
pub fn monitor_share(painter: &egui::Painter, rect: Rect, colour: Color32) {
    monitor(painter, rect, colour);
    let f = field(rect);
    let s = stroke(f, colour);
    let tip = egui::pos2(f.center().x, f.top() + f.height() * 0.20);
    let tail = egui::pos2(f.center().x, f.top() + f.height() * 0.52);
    painter.line_segment([tail, tip], s);
    let wing = f.width() * 0.13;
    painter.line_segment([tip, egui::pos2(tip.x - wing, tip.y + wing)], s);
    painter.line_segment([tip, egui::pos2(tip.x + wing, tip.y + wing)], s);
}

/// A frame with an arrow leaving its corner: show this in a window of its own.
pub fn pop_out(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    // The frame, with its top-right corner left open for the arrow to leave through.
    let frame = Rect::from_min_max(
        egui::pos2(f.left(), f.top() + f.height() * 0.18),
        egui::pos2(f.right() - f.width() * 0.18, f.bottom()),
    );
    painter.line_segment([frame.left_top(), frame.left_bottom()], s);
    painter.line_segment([frame.left_bottom(), frame.right_bottom()], s);
    painter.line_segment([frame.right_bottom(), egui::pos2(frame.right(), frame.center().y)], s);
    painter.line_segment([frame.left_top(), egui::pos2(frame.center().x, frame.top())], s);
    // The arrow, out through the corner.
    let tip = egui::pos2(f.right(), f.top());
    let tail = egui::pos2(f.center().x + f.width() * 0.02, f.center().y - f.height() * 0.02);
    painter.line_segment([tail, tip], s);
    let wing = f.width() * 0.26;
    painter.line_segment([tip, egui::pos2(tip.x - wing, tip.y)], s);
    painter.line_segment([tip, egui::pos2(tip.x, tip.y + wing)], s);
}

/// A gear, as a circle and a ring of teeth.
pub fn gear(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    let centre = f.center();
    let inner = f.width() * 0.20;
    let outer = f.width() * 0.42;
    painter.circle_stroke(centre, inner, s);
    for i in 0..8 {
        let angle = std::f32::consts::TAU * i as f32 / 8.0;
        let (sin, cos) = angle.sin_cos();
        painter.line_segment(
            [
                egui::pos2(centre.x + cos * inner * 1.35, centre.y + sin * inner * 1.35),
                egui::pos2(centre.x + cos * outer, centre.y + sin * outer),
            ],
            s,
        );
    }
}

/// A `#`, for a text channel.
pub fn hash(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    // Slanted, as the glyph is: two vertical-ish strokes leaning right, two horizontals.
    let lean = f.width() * 0.08;
    for x in [f.width() * 0.34, f.width() * 0.62] {
        painter.line_segment(
            [
                egui::pos2(f.left() + x + lean, f.top() + f.height() * 0.12),
                egui::pos2(f.left() + x - lean, f.bottom() - f.height() * 0.12),
            ],
            s,
        );
    }
    for y in [f.height() * 0.36, f.height() * 0.64] {
        painter.line_segment(
            [
                egui::pos2(f.left() + f.width() * 0.14, f.top() + y),
                egui::pos2(f.right() - f.width() * 0.14, f.top() + y),
            ],
            s,
        );
    }
}

/// A loudspeaker with two waves, for a voice channel.
pub fn speaker(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    // The cone, as a filled triangle-ish path — filled rather than stroked because at 14
    // points a stroked cone is three lines that read as noise.
    let left = f.left() + f.width() * 0.10;
    let throat = f.left() + f.width() * 0.32;
    let mouth = f.left() + f.width() * 0.52;
    painter.add(Shape::convex_polygon(
        vec![
            egui::pos2(left, f.center().y - f.height() * 0.12),
            egui::pos2(throat, f.center().y - f.height() * 0.12),
            egui::pos2(mouth, f.center().y - f.height() * 0.32),
            egui::pos2(mouth, f.center().y + f.height() * 0.32),
            egui::pos2(throat, f.center().y + f.height() * 0.12),
            egui::pos2(left, f.center().y + f.height() * 0.12),
        ],
        colour,
        Stroke::NONE,
    ));

    for (i, scale) in [0.30f32, 0.46].iter().enumerate() {
        let box_ = Rect::from_center_size(
            egui::pos2(mouth, f.center().y),
            egui::vec2(f.width() * scale * 2.0, f.height() * scale * 2.0),
        );
        let spread = if i == 0 { 0.7 } else { 0.9 };
        painter.add(arc(box_, -spread, spread, s));
    }
}

/// A paperclip, for attaching a file.
pub fn paperclip(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    // Two nested hooks, drawn as a polyline with rounded ends. Approximated with straight
    // segments and arcs at the turns, which at icon sizes is indistinguishable from a real
    // bezier and much easier to reason about.
    let x1 = f.left() + f.width() * 0.30;
    let x2 = f.left() + f.width() * 0.62;
    let top = f.top() + f.height() * 0.16;
    let bottom = f.bottom() - f.height() * 0.24;

    painter.line_segment([egui::pos2(x1, top + f.height() * 0.10), egui::pos2(x1, bottom)], s);
    painter.line_segment([egui::pos2(x2, top), egui::pos2(x2, bottom - f.height() * 0.16)], s);

    let cap = Rect::from_center_size(
        egui::pos2((x1 + x2) / 2.0, top + f.height() * 0.10),
        egui::vec2(x2 - x1, f.height() * 0.20),
    );
    painter.add(arc(cap, std::f32::consts::PI, std::f32::consts::TAU, s));

    let hook = Rect::from_center_size(
        egui::pos2((x1 + x2) / 2.0, bottom - f.height() * 0.16),
        egui::vec2(x2 - x1, f.height() * 0.32),
    );
    painter.add(arc(hook, 0.0, std::f32::consts::PI, s));
}

/// An upward arrow, for sending.
pub fn send(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    let tip = egui::pos2(f.center().x, f.top() + f.height() * 0.16);
    painter.line_segment([egui::pos2(f.center().x, f.bottom() - f.height() * 0.14), tip], s);
    let wing = f.width() * 0.26;
    painter.line_segment([tip, egui::pos2(tip.x - wing, tip.y + wing)], s);
    painter.line_segment([tip, egui::pos2(tip.x + wing, tip.y + wing)], s);
}

/// A cross, for closing and cancelling.
pub fn close(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect).shrink(field(rect).width() * 0.18);
    let s = stroke(f, colour);
    painter.line_segment([f.left_top(), f.right_bottom()], s);
    painter.line_segment([f.right_top(), f.left_bottom()], s);
}

/// A telephone receiver tipped over: leave the call.
pub fn hang_up(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    // A handset as an arc with two end caps, rotated a little so it reads as "down".
    let body = Rect::from_center_size(f.center(), egui::vec2(f.width() * 0.78, f.height() * 0.78));
    painter.add(arc(body, std::f32::consts::PI * 1.15, std::f32::consts::PI * 1.85, s));
    let radius = f.width() * 0.11;
    for angle in [std::f32::consts::PI * 1.15, std::f32::consts::PI * 1.85] {
        let (sin, cos) = angle.sin_cos();
        painter.circle_filled(
            egui::pos2(f.center().x + cos * body.width() / 2.0, f.center().y + sin * body.height() / 2.0),
            radius,
            colour,
        );
    }
}

/// A person, for the member list.
pub fn person(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    painter.circle_stroke(
        egui::pos2(f.center().x, f.top() + f.height() * 0.30),
        f.width() * 0.19,
        s,
    );
    let shoulders = Rect::from_center_size(
        egui::pos2(f.center().x, f.bottom() - f.height() * 0.02),
        egui::vec2(f.width() * 0.66, f.height() * 0.62),
    );
    painter.add(arc(shoulders, std::f32::consts::PI, std::f32::consts::TAU, s));
}

/// A downward arrow into a tray: a download, or a file offered.
pub fn download(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    let tip = egui::pos2(f.center().x, f.top() + f.height() * 0.56);
    painter.line_segment([egui::pos2(f.center().x, f.top() + f.height() * 0.10), tip], s);
    let wing = f.width() * 0.22;
    painter.line_segment([tip, egui::pos2(tip.x - wing, tip.y - wing)], s);
    painter.line_segment([tip, egui::pos2(tip.x + wing, tip.y - wing)], s);
    let y = f.bottom() - f.height() * 0.12;
    painter.line_segment(
        [egui::pos2(f.left() + f.width() * 0.14, y), egui::pos2(f.right() - f.width() * 0.14, y)],
        s,
    );
}

/// Draw a line through an icon, from lower-left to upper-right.
fn slash(painter: &egui::Painter, rect: Rect, colour: Color32) {
    let f = field(rect);
    let s = stroke(f, colour);
    painter.line_segment(
        [
            egui::pos2(f.left() + f.width() * 0.12, f.bottom() - f.height() * 0.12),
            egui::pos2(f.right() - f.width() * 0.12, f.top() + f.height() * 0.12),
        ],
        s,
    );
}

/// An elliptical arc inside `rect`, from `from` to `to` radians (clockwise, screen axes).
///
/// egui has no arc primitive, so this is a polyline. The segment count comes from the arc's
/// *length* rather than being fixed: a fixed count makes a small arc a wasteful sixteen-gon
/// and a large one a visible polygon.
fn arc(rect: Rect, from: f32, to: f32, stroke: Stroke) -> Shape {
    let radius = (rect.width() + rect.height()) / 4.0;
    let sweep = (to - from).abs();
    let steps = ((radius * sweep / 1.5).ceil() as usize).clamp(6, 64);
    let points: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let angle = from + (to - from) * i as f32 / steps as f32;
            let (sin, cos) = angle.sin_cos();
            egui::pos2(
                rect.center().x + cos * rect.width() / 2.0,
                rect.center().y + sin * rect.height() / 2.0,
            )
        })
        .collect();
    Shape::line(points, stroke)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drawing_field_is_a_centred_square_inside_the_box() {
        // A wide box: the icon stays square and centred rather than stretching, which is
        // what lets a caller reserve whatever rectangle its layout produced.
        let wide = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 20.0));
        let f = field(wide);
        assert!((f.width() - f.height()).abs() < 0.01, "{f:?}");
        assert!((f.center().x - wide.center().x).abs() < 0.01);
        assert!(f.width() <= 20.0);
    }

    #[test]
    fn the_stroke_scales_but_stays_visible() {
        let colour = Color32::WHITE;
        let tiny = stroke(Rect::from_min_size(Pos2::ZERO, Vec2::splat(8.0)), colour);
        let large = stroke(Rect::from_min_size(Pos2::ZERO, Vec2::splat(200.0)), colour);
        assert!(tiny.width >= 1.0, "a hairline would vanish on a low-DPI display");
        assert!(large.width <= 2.5, "an icon is not a logo");
        assert!(large.width >= tiny.width);
    }

    /// The segment count has to follow the arc, or a small one is wasteful and a big one is
    /// visibly a polygon.
    #[test]
    fn arcs_use_more_segments_when_they_are_bigger() {
        let count = |side: f32, sweep: f32| {
            let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(side));
            match arc(rect, 0.0, sweep, Stroke::new(1.0, Color32::WHITE)) {
                Shape::Path(path) => path.points.len(),
                other => panic!("expected a path, got {other:?}"),
            }
        };
        assert!(count(200.0, std::f32::consts::TAU) > count(20.0, std::f32::consts::TAU));
        assert!(count(100.0, std::f32::consts::TAU) > count(100.0, 0.5));
        // And never degenerate, however small.
        assert!(count(1.0, 0.01) >= 7);
        assert!(count(10_000.0, std::f32::consts::TAU) <= 65);
    }
}
