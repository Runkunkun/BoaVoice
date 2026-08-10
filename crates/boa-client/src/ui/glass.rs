//! The glass surfaces every panel in the app is built from.
//!
//! Real frosted glass in this window comes from the platform (see
//! [`crate::platform`]) — it blurs what is behind the *window*. What this module
//! adds is the part a compositor cannot know about: the impression that panels
//! are physical sheets stacked above that blur.
//!
//! Three cues do almost all of it, and they are worth naming because they are
//! what separates "glass" from "a translucent rectangle":
//!
//! * a **lit top edge**. Light in macOS comes from above, so the upper rim of a
//!   raised sheet catches it and the lower rim does not. A uniform border reads
//!   as a drawn outline; a rim that fades from bright at the top to nothing by
//!   the middle reads as a bevel.
//! * a **tint that darkens downward**, very slightly, which is the same lighting
//!   model applied to the face rather than the edge.
//! * **restraint about shadows**. A blurred drop shadow over a blurred backdrop
//!   muddies both, so depth here comes from the rim, not from shade.
//!
//! Everything is painted rather than composed from egui's `Frame` alone, because
//! `Frame` gives one fill and one uniform stroke and the whole effect lives in
//! those not being uniform.

use egui::{Color32, CornerRadius, Rect, Stroke, StrokeKind, Ui};

use crate::theme;

/// How far down from the top edge the lit rim fades to nothing, in points.
///
/// An absolute depth, not a fraction of the panel. A fraction is the obvious
/// choice and the wrong one: it turns a bevel into a sixty-pixel wash on a tall
/// sidebar while leaving a toolbar with almost none. A bevel is a property of
/// the edge, so it is the same few points wherever it appears.
const RIM_DEPTH: f32 = 10.0;

/// Depth of the shadow cast into the top of a recessed well, in points.
///
/// Shallower than [`RIM_DEPTH`] because wells are small — a text field is thirty
/// points tall and a ten-point shadow would darken a third of it.
const WELL_DEPTH: f32 = 4.0;

/// A rim effect must never cover more than this fraction of its surface, for the
/// case where the surface is shorter than the depth above.
const RIM_MAX_FRACTION: f32 = 0.45;

// A well is shaded at the top and a pane is lit there; if the shading ran as deep
// as the lighting, the two would stop reading as different depths. Both are
// constants, so this is checked when the crate is compiled rather than when it is
// tested.
const _: () = assert!(WELL_DEPTH < RIM_DEPTH);

/// Steps in the rim highlight, which only ever covers a short band.
const RIM_STEPS: usize = 12;

/// Target height of one band in the face gradient, in points.
///
/// The gradient is drawn as a stack of bands, and a fixed *count* would make the
/// band height scale with the panel — a tall card ends up with visible steps.
/// Fixing the height instead and deriving the count keeps the ramp smooth at any
/// size, at a cost that is still trivial (a few dozen rects per panel).
const BAND_HEIGHT: f32 = 3.0;

/// Ceiling on band count, so an absurdly tall panel cannot flood the paint list.
const MAX_BANDS: usize = 96;

/// A raised sheet: sidebar, the voice bar, a dialog.
pub fn panel(ui: &Ui, rect: Rect) {
    ui.painter().add(pane_shape(rect, theme::GLASS, theme::R_PANEL));
}

/// A sheet one step further forward — a card inside a panel.
pub fn card(ui: &Ui, rect: Rect) {
    ui.painter()
        .add(pane_shape(rect, theme::GLASS_HIGH, theme::R_CONTROL));
}

/// Reserve a slot for a card, to be filled once its contents have been laid out.
///
/// egui paints in call order, so a panel whose height depends on what goes
/// inside it cannot be drawn first. Reserving an index up front and writing the
/// shape into it afterwards puts the card *behind* its contents without a second
/// layout pass.
pub fn reserve(ui: &Ui) -> egui::layers::ShapeIdx {
    ui.painter().add(egui::Shape::Noop)
}

/// Fill a slot from [`reserve`] with a card covering `rect`.
pub fn fill_card(ui: &Ui, slot: egui::layers::ShapeIdx, rect: Rect) {
    ui.painter()
        .set(slot, pane_shape(rect, theme::GLASS_HIGH, theme::R_CONTROL));
}

/// A recessed well: list backgrounds, text fields, the message composer.
///
/// Inverted lighting. A sunken surface is shaded at the *top* (where the wall
/// above it casts) and catches light along the bottom, which is exactly the
/// opposite of [`pane_shape`] — and is why a well and a card read as different
/// depths even at the same fill.
pub fn well(ui: &Ui, rect: Rect, radius: CornerRadius) {
    let mut shapes = vec![egui::Shape::rect_filled(rect, radius, theme::GLASS_LOW)];

    let depth = rim_depth(WELL_DEPTH, rect.height());
    for step in 0..RIM_STEPS {
        let t = step as f32 / RIM_STEPS as f32;
        let y = rect.top() + t * depth;
        let alpha = (26.0 * (1.0 - t)) as u8;
        if alpha == 0 {
            continue;
        }
        shapes.push(egui::Shape::line_segment(
            [egui::pos2(rect.left() + 2.0, y), egui::pos2(rect.right() - 2.0, y)],
            Stroke::new(1.0, theme::alpha(Color32::BLACK, alpha)),
        ));
    }

    shapes.push(egui::Shape::rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, theme::RIM),
        StrokeKind::Inside,
    ));
    ui.painter().add(egui::Shape::Vec(shapes));
}

/// Build one glass sheet as a shape.
///
/// Returned rather than painted so callers can either draw it immediately or
/// slot it in behind content laid out later — see [`reserve`].
pub fn pane_shape(rect: Rect, fill: Color32, radius: CornerRadius) -> egui::Shape {
    let height = rect.height().max(1.0);
    let bands = ((height / BAND_HEIGHT).ceil() as usize).clamp(1, MAX_BANDS);
    let mut shapes = Vec::with_capacity(bands + RIM_STEPS + 2);

    // The face. A single fill, then a light-to-dark ramp down from the top edge —
    // the same light direction as the rim, applied to the surface.
    shapes.push(egui::Shape::rect_filled(rect, radius, fill));

    // Bands, not thick lines. A stroked line straddles its centre, so a stack of
    // them either overlaps (double-blending into visible seams) or leaves gaps.
    // Filling from one boundary to the next abuts exactly.
    for band in 0..bands {
        let top = rect.top() + height * band as f32 / bands as f32;
        let bottom = rect.top() + height * (band + 1) as f32 / bands as f32;
        // Sample the ramp at the band's centre so the average is right.
        let t = (band as f32 + 0.5) / bands as f32;
        let alpha = (10.0 * (1.0 - t) * 0.5).round() as u8;
        if alpha == 0 {
            continue;
        }
        // Inset by a point so the square band cannot poke out of the rounded
        // corners; at these alphas the missing sliver is invisible.
        shapes.push(egui::Shape::rect_filled(
            Rect::from_min_max(
                egui::pos2(rect.left() + 1.0, top),
                egui::pos2(rect.right() - 1.0, bottom),
            ),
            CornerRadius::ZERO,
            theme::alpha(Color32::WHITE, alpha),
        ));
    }

    // The rim. A full inset outline first, so the sides and bottom get their
    // hairline...
    shapes.push(egui::Shape::rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, theme::RIM),
        StrokeKind::Inside,
    ));

    // ...then the top edge is brightened over it, fading out as it descends the
    // shoulders of the rounded corners. This is the cue that sells the bevel.
    let inset = rect.shrink(0.5);
    let radius_px = f32::from(radius.nw).min(inset.width() / 2.0);
    let depth = rim_depth(RIM_DEPTH, inset.height());
    for step in 0..RIM_STEPS {
        let t = step as f32 / RIM_STEPS as f32;
        let y = inset.top() + t * depth;
        // Inside the corner radius the highlight has to pull in with the curve,
        // or it would run out past the panel's edge.
        let dy = (y - (inset.top() + radius_px)).min(0.0);
        let dx = corner_inset(radius_px, dy);
        let alpha = (46.0 * (1.0 - t)) as u8;
        if alpha == 0 {
            continue;
        }
        shapes.push(egui::Shape::line_segment(
            [
                egui::pos2(inset.left() + dx, y),
                egui::pos2(inset.right() - dx, y),
            ],
            Stroke::new(1.0, theme::alpha(Color32::WHITE, alpha)),
        ));
    }

    egui::Shape::Vec(shapes)
}

/// The depth a rim effect actually gets on a surface `height` tall.
fn rim_depth(preferred: f32, height: f32) -> f32 {
    preferred.min(height.max(1.0) * RIM_MAX_FRACTION)
}

/// How far in from the side the rim highlight sits, `dy` above the end of the
/// corner arc (`dy` is zero or negative).
fn corner_inset(radius: f32, dy: f32) -> f32 {
    radius - (radius * radius - dy * dy).max(0.0).sqrt()
}

/// A hairline separator, for splitting a list into sections.
pub fn divider(ui: &mut Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 9.0), egui::Sense::hover());
    let y = rect.center().y.round() + 0.5;
    ui.painter().line_segment(
        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
        Stroke::new(1.0, theme::alpha(Color32::WHITE, 16)),
    );
}

/// Wash a rect in the selection or hover tint.
///
/// Used by list rows, which cannot use egui's own selectable background because
/// they draw their own multi-line layout.
pub fn row_highlight(ui: &Ui, rect: Rect, selected: bool, hovered: bool) {
    let fill = match (selected, hovered) {
        (true, _) => theme::SELECTED,
        (false, true) => theme::HOVER,
        (false, false) => return,
    };
    ui.painter().rect_filled(rect, theme::R_CHIP, fill);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corner inset must follow the arc: at the very top of a rounded panel
    /// the highlight starts a full radius in from each side, and by the time it
    /// reaches the bottom of the curve it has reached the edge.
    #[test]
    fn the_rim_highlight_follows_the_corner_radius() {
        let radius = 16.0_f32;
        let inset_at = |y_offset: f32| corner_inset(radius, (y_offset - radius).min(0.0));

        // At the top edge the highlight is inset by the whole radius.
        assert!((inset_at(0.0) - radius).abs() < 0.01);
        // Level with the end of the curve it is flush with the side.
        assert!(inset_at(radius).abs() < 0.01);
        // And below the curve it stays flush rather than going negative.
        assert!(inset_at(radius * 3.0).abs() < 0.01);
        // In between it is somewhere sensible.
        let middle = inset_at(radius / 2.0);
        assert!(middle > 0.0 && middle < radius, "{middle}");
    }

    /// The highlight is an edge treatment. On a tall panel it must stay a thin
    /// band near the top rather than growing with the surface — the bug this
    /// replaced turned the sidebar's bevel into a sixty-point gradient wash.
    #[test]
    fn the_rim_highlight_is_a_fixed_depth_not_a_fraction() {
        let short = rim_depth(RIM_DEPTH, 40.0);
        let tall = rim_depth(RIM_DEPTH, 700.0);
        assert_eq!(short, tall, "depth must not scale with the panel");
        assert_eq!(tall, RIM_DEPTH);

        // Except on a surface too short to hold it, where it is capped so the
        // highlight cannot swamp the whole thing.
        let tiny = rim_depth(RIM_DEPTH, 8.0);
        assert!(tiny < RIM_DEPTH);
        assert!(tiny <= 8.0 * RIM_MAX_FRACTION + 0.001);
    }

}
