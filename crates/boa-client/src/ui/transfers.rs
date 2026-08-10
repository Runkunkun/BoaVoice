//! The strip above the composer: files offered to you, and files in flight.
//!
//! It sits in the channel view rather than in a window of its own, because a transfer needs both
//! people present and an offer that appears in a panel nobody has open is an offer that expires. It
//! is only drawn when there is something to draw.
//!
//! The one piece of judgement here is what a transfer's row says while it is running. "42%" is not
//! enough — a transfer that has stalled shows 42% for ten minutes — so a row carries the rate as
//! well, which goes to zero when nothing is moving.

use egui::Ui;

use crate::theme;
use crate::ui::{glass, icons, widgets, Action};

/// A transfer the interface is following.
pub struct Active {
    pub id: u64,
    /// Who it is going to or coming from.
    pub peer: boa_proto::Id,
    pub name: String,
    pub outgoing: bool,
    pub done: u64,
    pub total: u64,
    /// Set once the two sides have connected, and whether it was direct.
    pub direct: Option<bool>,
    /// When progress was last seen, for the rate.
    pub at: std::time::Instant,
    pub last_done: u64,
    /// Bytes per second, smoothed.
    pub rate: f64,
}

impl Active {
    pub fn new(id: u64, peer: boa_proto::Id, name: String, outgoing: bool, total: u64) -> Active {
        Active {
            id,
            peer,
            name,
            outgoing,
            done: 0,
            total,
            direct: None,
            at: std::time::Instant::now(),
            last_done: 0,
            rate: 0.0,
        }
    }

    /// Record progress and update the rate.
    pub fn advance(&mut self, done: u64, total: u64) {
        self.total = total.max(1);
        let elapsed = self.at.elapsed().as_secs_f64();
        // Only recompute over a reasonable interval: the callback fires per chunk, and dividing a
        // few kilobytes by a few microseconds produces a number that jumps between 0 and 4 GB/s.
        if elapsed >= 0.35 {
            let moved = done.saturating_sub(self.last_done) as f64;
            let instant = moved / elapsed;
            // Smoothed, so the figure is readable rather than flickering — but weighted towards the
            // new value, so a stall shows up within a second rather than fading out over ten.
            self.rate = if self.rate == 0.0 { instant } else { self.rate * 0.4 + instant * 0.6 };
            self.at = std::time::Instant::now();
            self.last_done = done;
        }
        self.done = done;
    }

    fn fraction(&self) -> f32 {
        (self.done as f32 / self.total.max(1) as f32).clamp(0.0, 1.0)
    }
}

/// Draw the strip. Returns nothing when there is nothing to show.
pub fn show(
    ui: &mut Ui,
    state: &crate::state::State,
    active: &[Active],
    channel: boa_proto::Id,
) -> Option<Action> {
    let offers: Vec<_> = state.offers.iter().filter(|(_, offer)| offer.channel == channel).collect();
    if offers.is_empty() && active.is_empty() {
        return None;
    }

    let mut action = None;
    ui.add_space(4.0);

    for (from, offer) in offers {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 40.0),
            egui::Sense::hover(),
        );
        glass::card(ui, rect);

        let icon = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            egui::Vec2::splat(18.0),
        );
        icons::download(ui.painter(), icon, theme::TRANSFER);

        ui.painter().text(
            egui::pos2(rect.left() + 38.0, rect.center().y - 7.0),
            egui::Align2::LEFT_CENTER,
            format!("{} wants to send you {}", state.label(*from), offer.name),
            egui::FontId::proportional(12.0),
            theme::TEXT,
        );
        ui.painter().text(
            egui::pos2(rect.left() + 38.0, rect.center().y + 8.0),
            egui::Align2::LEFT_CENTER,
            format!("{} — straight from their machine, not through the server", widgets::bytes(offer.size)),
            egui::FontId::proportional(10.0),
            theme::TEXT_FAINT,
        );

        // The buttons are laid out from the right edge by hand: the row is painted absolutely, so
        // there is no layout cursor to hang them off.
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(
            egui::Rect::from_min_max(
                egui::pos2(rect.right() - 190.0, rect.top() + 5.0),
                egui::pos2(rect.right() - 8.0, rect.bottom() - 5.0),
            ),
        ));
        child.horizontal(|ui| {
            if widgets::pill_button(ui, "Save", true).clicked() {
                action = Some(Action::AcceptOffer(*from, offer.code.clone()));
            }
            if widgets::pill_button(ui, "No thanks", false).clicked() {
                action = Some(Action::DeclineOffer(*from, offer.code.clone()));
            }
        });
        ui.add_space(4.0);
    }

    for transfer in active {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 40.0),
            egui::Sense::hover(),
        );
        glass::card(ui, rect);

        let direction = if transfer.outgoing { "→" } else { "←" };
        ui.painter().text(
            egui::pos2(rect.left() + 12.0, rect.center().y - 7.0),
            egui::Align2::LEFT_CENTER,
            format!("{direction} {} · {}", transfer.name, state.label(transfer.peer)),
            egui::FontId::proportional(12.0),
            theme::TEXT,
        );

        let detail = match transfer.direct {
            // Said out loud, because "directly" is the feature and a relayed transfer is the
            // fallback — somebody watching a slow transfer should know which they got.
            Some(true) => "direct".to_string(),
            Some(false) => "through the relay".to_string(),
            None => "waiting for the other side…".to_string(),
        };
        let rate = if transfer.rate > 1.0 {
            format!(" · {}/s", widgets::bytes(transfer.rate as u64))
        } else {
            String::new()
        };
        ui.painter().text(
            egui::pos2(rect.left() + 12.0, rect.center().y + 8.0),
            egui::Align2::LEFT_CENTER,
            format!(
                "{} of {}{rate} · {detail}",
                widgets::bytes(transfer.done),
                widgets::bytes(transfer.total)
            ),
            egui::FontId::proportional(10.0),
            theme::TEXT_FAINT,
        );

        // The bar, along the bottom edge of the card rather than as a widget: it is a progress
        // indicator on a row, not something to interact with.
        let bar = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 1.0, rect.bottom() - 3.0),
            egui::pos2(rect.right() - 1.0, rect.bottom() - 1.0),
        );
        ui.painter().rect_filled(bar, theme::R_CHIP, theme::alpha(theme::TEXT, 20));
        let filled = egui::Rect::from_min_size(
            bar.min,
            egui::vec2(bar.width() * transfer.fraction(), bar.height()),
        );
        ui.painter().rect_filled(filled, theme::R_CHIP, theme::TRANSFER);

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
            egui::pos2(rect.right() - 34.0, rect.top() + 5.0),
            egui::pos2(rect.right() - 6.0, rect.bottom() - 6.0),
        )));
        if widgets::icon_button(&mut child, icons::close, 24.0, "Cancel this transfer").clicked() {
            action = Some(Action::CancelTransfer(transfer.id));
        }
        ui.add_space(4.0);
    }

    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_ignores_intervals_too_short_to_measure() {
        let mut transfer = Active::new(1, boa_proto::Id(2), "big.zip".into(), true, 1_000_000);
        // A callback moments after the last one: dividing a few kilobytes by a few microseconds
        // would produce a number in the gigabytes.
        transfer.advance(1_000, 1_000_000);
        assert_eq!(transfer.rate, 0.0);
        assert_eq!(transfer.done, 1_000);
    }

    #[test]
    fn the_rate_appears_once_there_is_an_interval_to_divide_by() {
        let mut transfer = Active::new(1, boa_proto::Id(2), "big.zip".into(), true, 1_000_000);
        transfer.at = std::time::Instant::now() - std::time::Duration::from_secs(1);
        transfer.advance(500_000, 1_000_000);
        // Half a megabyte in a second, give or take the smoothing.
        assert!(transfer.rate > 100_000.0, "{}", transfer.rate);
    }

    #[test]
    fn progress_is_a_fraction_and_cannot_exceed_one() {
        let mut transfer = Active::new(1, boa_proto::Id(2), "x".into(), false, 100);
        assert_eq!(transfer.fraction(), 0.0);
        transfer.advance(50, 100);
        assert!((transfer.fraction() - 0.5).abs() < 0.01);
        // A sender that overshoots its own advertised size must not draw past the end of the bar.
        transfer.advance(500, 100);
        assert_eq!(transfer.fraction(), 1.0);

        // And a zero-length file is complete rather than a division by zero.
        let empty = Active::new(2, boa_proto::Id(2), "empty".into(), false, 0);
        assert!(empty.fraction().is_finite());
    }
}
