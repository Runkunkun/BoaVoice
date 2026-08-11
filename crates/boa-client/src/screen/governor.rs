//! Finding the bitrate the connection can actually carry.
//!
//! A screen share configured at 16 Mbit/s on a link that can carry 8 is not a share that looks
//! slightly worse. It is a share that does not work at all: every second picture arrives with a hole in
//! it, and a picture with a hole is a picture the decoder cannot use. Nothing at the receiving end can
//! repair that, because the bytes never left the sender's building — and the sender has no idea,
//! because UDP does not report loss. That is the whole problem, and it is why every video call in the
//! world has a control loop like this one.
//!
//! The loop here is deliberately the simplest thing that works, and it is the same shape as the
//! loss-based half of WebRTC's: the watcher reports how much of the picture arrived, and the sender
//! **comes down quickly when loss appears and creeps back up when it does not**. Down fast because loss
//! means the queue is already full and every further packet is wasted; up slowly because the only way
//! to discover the ceiling is to walk into it, and walking into it costs a stutter.
//!
//! What this is not: a bandwidth *estimator*. WebRTC measures the delay gradient between packets and
//! predicts congestion before it causes loss, which is better and much more machinery. This reacts to
//! loss that has already happened. The difference is a second of stutter after the link degrades,
//! rather than none — against a share that is permanently broken, which is what there was before.

/// Loss above this is treated as "the link cannot carry this", as a percentage of pictures.
///
/// Ten percent. Chosen against what loss *does* here rather than against a feeling: a lost fragment
/// costs a whole picture, and a picture lost every ten is visible stutter. Below that, the stream is
/// watchable and coming down would trade a working picture for a worse one.
const TOO_MUCH: u32 = 10;

/// Loss below this counts as "there is room to grow".
///
/// Two percent, not zero. A link at exactly its capacity loses the occasional packet, and a controller
/// that only grows on a perfect second would sit far below the ceiling forever.
const COMFORTABLE: u32 = 2;

/// How hard to come down: keep this much of the current rate.
const BACK_OFF: f32 = 0.7;

/// How gently to go back up: add this much of the current rate.
///
/// Five percent a second, so recovering from a halving takes about fifteen seconds. Slower than it
/// sounds sensible and slower than it feels satisfying, both on purpose: every step up that turns out
/// to be too much costs the viewer a stutter, and the ceiling is usually near where it was last time.
const CREEP: f32 = 0.05;

/// Never go below this, in kbit/s.
///
/// 400 kbit/s is a legible 1080p screen of text at a low frame rate — ugly, and far better than
/// nothing. Below it the picture stops being useful, and if the link cannot carry this much then the
/// honest answer is to say so rather than to keep dividing.
const FLOOR: u32 = 400;

/// Decides what bitrate to encode at, from what watchers report.
pub struct Governor {
    /// What the user asked for. Never exceeded: this is a controller, not a suggestion box.
    ceiling: u32,
    /// What the encoder is being told now.
    target: u32,
    /// The worst loss any single watcher reported in the last round.
    ///
    /// The *worst*, not the average: with two people watching, one on a fast link and one on a slow
    /// one, averaging leaves the slow one with a broken picture forever. Somebody has to be
    /// disappointed and it should be the one who can afford it.
    worst: u32,
    /// Whether anybody asked for a keyframe.
    keyframe_wanted: bool,
}

/// What the sender should do about the reports it has received.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decision {
    /// The bitrate to encode at, in kbit/s.
    pub kbps: u32,
    /// Whether it changed enough to be worth telling the encoder about.
    pub changed: bool,
    /// Whether a keyframe should be forced now.
    pub keyframe: bool,
}

impl Governor {
    /// Start at the configured bitrate, which is where an unloaded link should sit.
    pub fn new(ceiling: u32) -> Governor {
        Governor {
            ceiling: ceiling.max(FLOOR),
            target: ceiling.max(FLOOR),
            worst: 0,
            keyframe_wanted: false,
        }
    }

    /// The bitrate currently being asked for.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// The loss the last round reported, for the interface to show.
    pub fn loss(&self) -> u32 {
        self.worst
    }

    /// Take one watcher's report.
    pub fn report(&mut self, received: u32, lost: u32, want_keyframe: bool) {
        let total = received + lost;
        // Under five pictures is not a measurement. A single lost picture out of two is 50% loss and
        // means nothing; acting on it would make a controller that lurches at the start of every share.
        if total >= 5 {
            let loss = lost.saturating_mul(100) / total;
            self.worst = self.worst.max(loss);
        }
        self.keyframe_wanted |= want_keyframe;
    }

    /// Decide, and start the next round. Called once a second.
    pub fn decide(&mut self) -> Decision {
        let before = self.target;
        if self.worst >= TOO_MUCH {
            // Down. Multiplicatively, because loss means the bottleneck is already overrun and shaving
            // a few percent off would only overrun it slightly less.
            self.target = ((self.target as f32 * BACK_OFF) as u32).max(FLOOR);
        } else if self.worst <= COMFORTABLE {
            // Up, gently, and never past what was asked for.
            self.target = (self.target + (self.target as f32 * CREEP) as u32 + 1).min(self.ceiling);
        }

        // A change smaller than a twentieth is not worth reconfiguring the encoder for — it would mean
        // a property write every second for no visible difference.
        let changed = before.abs_diff(self.target) * 20 > before;
        let keyframe = std::mem::take(&mut self.keyframe_wanted);
        self.worst = 0;
        Decision { kbps: self.target, changed, keyframe }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the whole thing: heavy loss brings the rate down fast, and a clean link brings it
    /// back up slowly without ever passing what the user asked for.
    #[test]
    fn it_comes_down_fast_and_goes_up_slowly() {
        let mut governor = Governor::new(16_000);
        assert_eq!(governor.target(), 16_000);

        // A third of the picture is not arriving.
        governor.report(60, 30, false);
        let down = governor.decide();
        assert!(down.changed);
        assert!(down.kbps < 12_000, "should have backed off hard, got {}", down.kbps);

        // Three more bad seconds, and it should be down near a tenth of where it started.
        for _ in 0..3 {
            governor.report(60, 30, false);
            governor.decide();
        }
        assert!(governor.target() < 5_000, "still too high: {}", governor.target());
        let bottom = governor.target();

        // Now the link is clean. Up, but not in one bound.
        governor.report(60, 0, false);
        let up = governor.decide();
        assert!(up.kbps > bottom);
        assert!(up.kbps < bottom * 2, "grew too fast: {bottom} → {}", up.kbps);

        // And after a long clean stretch it settles at the ceiling and stops.
        for _ in 0..300 {
            governor.report(60, 0, false);
            governor.decide();
        }
        assert_eq!(governor.target(), 16_000, "should return to what was asked for, and no further");
    }

    /// Loss in the comfortable band is left alone. A controller that reacted to every stray packet
    /// would oscillate around the ceiling instead of sitting at it.
    #[test]
    fn a_little_loss_is_not_a_reason_to_move() {
        let mut governor = Governor::new(8_000);
        governor.report(98, 2, false);
        let steady = governor.decide();
        assert_eq!(steady.kbps, 8_000);
        assert!(!steady.changed);

        // Five percent: worth neither growing nor shrinking.
        let mut governor = Governor::new(8_000);
        governor.report(95, 5, false);
        let held = governor.decide();
        assert_eq!(held.kbps, 8_000, "five percent should hold, not back off");
    }

    /// The slowest watcher decides. Averaging would leave them with a permanently broken picture while
    /// the numbers looked fine.
    #[test]
    fn the_worst_report_wins() {
        let mut governor = Governor::new(10_000);
        governor.report(100, 0, false); // a fast link
        governor.report(50, 40, false); // and somebody on hotel wifi
        let decision = governor.decide();
        assert!(decision.kbps < 8_000, "the slow watcher should have been heard: {}", decision.kbps);
    }

    /// A handful of pictures is not a measurement, and acting on it would make every share lurch in its
    /// first second.
    #[test]
    fn a_tiny_sample_is_ignored() {
        let mut governor = Governor::new(6_000);
        governor.report(1, 1, false);
        let decision = governor.decide();
        assert_eq!(decision.kbps, 6_000, "half of two pictures is not 50% loss");
    }

    /// It never divides its way into nothing, however bad the link is.
    #[test]
    fn there_is_a_floor() {
        let mut governor = Governor::new(16_000);
        for _ in 0..200 {
            governor.report(10, 90, false);
            governor.decide();
        }
        assert_eq!(governor.target(), FLOOR);
    }

    /// A keyframe request survives until it is acted on, and only once.
    #[test]
    fn a_keyframe_request_is_passed_on_once() {
        let mut governor = Governor::new(4_000);
        governor.report(20, 0, true);
        governor.report(20, 0, false);
        assert!(governor.decide().keyframe, "somebody asked for one");
        assert!(!governor.decide().keyframe, "and it should not be asked for forever");
    }

    /// The ceiling is the user's setting, and a controller that crept past it would be overriding them.
    #[test]
    fn the_users_setting_is_a_ceiling() {
        let mut governor = Governor::new(1_000);
        for _ in 0..100 {
            governor.report(100, 0, false);
            governor.decide();
        }
        assert_eq!(governor.target(), 1_000);
    }
}
