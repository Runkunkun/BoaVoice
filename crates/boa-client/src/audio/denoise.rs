//! Cleaning up the captured signal: gain, noise suppression, and a gate.
//!
//! Three stages that are often confused with each other, so it is worth being precise about what
//! each one does, because a user who cannot hear the difference will reach for the wrong one:
//!
//! * **Gain** makes everything louder, including the fan.
//! * **Noise suppression** removes steady broadband noise *from* speech. It runs while you talk, and
//!   it is what makes a laptop microphone in a room with a fan sound like a headset. It saves no
//!   bandwidth at all.
//! * **The gate** stops transmitting when there is nothing to transmit. It does nothing to the sound
//!   of your voice, and it is the only one of the three that saves bandwidth — and the only one that
//!   stops your room being audible to eight people while you are not talking.
//!
//! The suppressor is RNNoise, through [`nnnoiseless`]: a small recurrent network trained on speech
//! and noise, which is why it can tell a voice from a fan rather than merely subtracting a measured
//! noise floor. It has one hard requirement — 480-sample frames at 48 kHz, which is 10 ms — and one
//! trap: it works on **16-bit-scaled floats**, not on the ±1.0 range everything else in this app
//! uses. Feeding it ±1.0 samples produces silence with no error, which is the kind of bug that gets
//! diagnosed as a broken microphone.

use nnnoiseless::DenoiseState;

/// Samples in one RNNoise frame, and therefore in one call to [`Cleanup::process`].
pub const FRAME: usize = 480;

/// What RNNoise expects a full-scale sample to be.
pub const RNNOISE_SCALE: f32 = 32_767.0;

/// The voice-activity probability above which RNNoise's own judgement counts as speech.
///
/// Only used to *help* the gate, never alone: the network is confident about a voice in noise and
/// unconfident about a whisper, and a gate driven purely by it clips quiet speech. The level
/// threshold is the decision; this widens it.
const VOICE_PROBABILITY: f32 = 0.55;

/// What one frame of processing produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outcome {
    /// Peak level after gain and suppression, 0…1. What the meter draws.
    pub level: f32,
    /// Whether this frame is being transmitted.
    pub open: bool,
    /// RNNoise's own opinion, or `None` when suppression is off.
    pub voice_probability: Option<f32>,
}

pub struct Cleanup {
    /// Boxed because `DenoiseState` is several hundred kilobytes of weights and state, which is not
    /// something to keep on a stack that an audio thread also uses.
    denoiser: Option<Box<DenoiseState<'static>>>,
    gate: Gate,
    /// Scratch, so a frame costs no allocation.
    scaled: [f32; FRAME],
    cleaned: [f32; FRAME],
}

impl Cleanup {
    pub fn new(threshold_db: f32, hang_ms: u32, suppress: bool) -> Cleanup {
        Cleanup {
            denoiser: suppress.then(DenoiseState::new),
            gate: Gate::new(threshold_db, hang_ms),
            scaled: [0.0; FRAME],
            cleaned: [0.0; FRAME],
        }
    }

    /// Turn suppression on or off without rebuilding everything else.
    ///
    /// Creating the state is the expensive part, so it is kept once created — switching back on
    /// mid-call must not allocate on the thread that has a 10 ms deadline.
    pub fn set_suppression(&mut self, on: bool) {
        match (on, self.denoiser.is_some()) {
            (true, false) => self.denoiser = Some(DenoiseState::new()),
            (false, true) => { /* kept, see above */ }
            _ => {}
        }
        self.gate.suppressing = on;
    }

    pub fn suppressing(&self) -> bool {
        self.gate.suppressing && self.denoiser.is_some()
    }

    pub fn set_threshold(&mut self, threshold_db: f32) {
        self.gate.threshold = amplitude_from_db(threshold_db);
    }

    pub fn set_hang(&mut self, hang_ms: u32) {
        self.gate.hang_frames = hang_frames(hang_ms);
    }

    /// Process one frame in place. `gain` is applied first; `forced` overrides the gate (push to
    /// talk, which should transmit a held key's silence rather than second-guessing it).
    pub fn process(&mut self, frame: &mut [f32; FRAME], gain: f32, forced: Option<bool>) -> Outcome {
        for sample in frame.iter_mut() {
            // Clamped, because gain above unity on an already-loud signal would otherwise wrap into
            // the codec as distortion that no amount of turning it down afterwards can undo.
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }

        let mut probability = None;
        if self.suppressing() {
            // The scale conversion RNNoise needs. Getting this wrong produces silence and no error.
            for (out, sample) in self.scaled.iter_mut().zip(frame.iter()) {
                *out = *sample * RNNOISE_SCALE;
            }
            if let Some(denoiser) = self.denoiser.as_mut() {
                probability = Some(denoiser.process_frame(&mut self.cleaned, &self.scaled));
                for (sample, cleaned) in frame.iter_mut().zip(self.cleaned.iter()) {
                    *sample = (cleaned / RNNOISE_SCALE).clamp(-1.0, 1.0);
                }
            }
        }

        let level = frame.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        let open = match forced {
            Some(held) => {
                // Push to talk means the key decides. The gate is still *run*, so its hang state
                // stays current and releasing the key does not leave it stale.
                self.gate.step(level, probability);
                held
            }
            None => self.gate.step(level, probability),
        };

        Outcome { level, open, voice_probability: probability }
    }
}

/// A noise gate with a hang time.
struct Gate {
    /// Peak amplitude, 0…1, below which a frame is not speech.
    threshold: f32,
    /// How many frames the gate stays open after the level drops.
    hang_frames: u32,
    /// Frames remaining before it closes.
    remaining: u32,
    suppressing: bool,
}

impl Gate {
    fn new(threshold_db: f32, hang_ms: u32) -> Gate {
        Gate {
            threshold: amplitude_from_db(threshold_db),
            hang_frames: hang_frames(hang_ms),
            remaining: 0,
            suppressing: true,
        }
    }

    /// Advance one frame; returns whether to transmit it.
    ///
    /// The hang time is the whole reason this is a state machine rather than a comparison. A gate
    /// that closes the instant the level drops cuts the ends off words — the tail of a vowel is
    /// quiet — and turns a sentence into a series of clipped fragments. It opens instantly and
    /// closes slowly, which is the asymmetry every gate needs.
    fn step(&mut self, level: f32, voice_probability: Option<f32>) -> bool {
        let loud = level >= self.threshold;
        // The network's opinion widens the gate but cannot open it on its own: RNNoise reports high
        // probability for a voice in the next room, and that is exactly what should not be sent.
        let voiced = voice_probability.is_some_and(|p| p >= VOICE_PROBABILITY)
            && level >= self.threshold * 0.5;

        if loud || voiced {
            self.remaining = self.hang_frames;
            return true;
        }
        if self.remaining > 0 {
            self.remaining -= 1;
            return true;
        }
        false
    }
}

/// Amplitude (0…1) for a level in dBFS.
///
/// Anything at or below −90 dB returns zero, which makes the gate permanently open — that is what
/// somebody who dragged the slider to the bottom meant.
pub fn amplitude_from_db(db: f32) -> f32 {
    if db <= -90.0 {
        return 0.0;
    }
    10.0_f32.powf(db / 20.0)
}

/// dBFS for an amplitude, floored at −90.
pub fn db_from_amplitude(amplitude: f32) -> f32 {
    if amplitude <= 0.000_03 {
        return -90.0;
    }
    20.0 * amplitude.log10()
}

fn hang_frames(hang_ms: u32) -> u32 {
    // One frame is 10 ms.
    hang_ms / 10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amplitude: f32) -> [f32; FRAME] {
        let mut frame = [0.0; FRAME];
        for (i, sample) in frame.iter_mut().enumerate() {
            // 440 Hz at 48 kHz, so the frame contains several whole cycles and its peak is the
            // amplitude asked for.
            *sample = amplitude * (i as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin();
        }
        frame
    }

    #[test]
    fn decibels_and_amplitudes_are_inverses() {
        for db in [-6.0, -20.0, -45.0, -60.0] {
            let amplitude = amplitude_from_db(db);
            assert!((db_from_amplitude(amplitude) - db).abs() < 0.01, "{db}");
        }
        assert_eq!(amplitude_from_db(0.0), 1.0);
        // The bottom of the slider means "never gate".
        assert_eq!(amplitude_from_db(-90.0), 0.0);
        assert_eq!(amplitude_from_db(-120.0), 0.0);
        assert_eq!(db_from_amplitude(0.0), -90.0);
    }

    #[test]
    fn the_gate_opens_on_speech_and_stays_shut_on_a_quiet_room() {
        let mut cleanup = Cleanup::new(-40.0, 0, false);

        let quiet = cleanup.process(&mut tone(0.001), 1.0, None);
        assert!(!quiet.open, "−60 dB is not speech");
        assert!(quiet.level < 0.01);

        let loud = cleanup.process(&mut tone(0.3), 1.0, None);
        assert!(loud.open);
        assert!(loud.level > 0.25);
    }

    /// The asymmetry that makes a gate usable: instant to open, slow to close. Without it the quiet
    /// tail of every word is cut off.
    #[test]
    fn the_gate_holds_open_for_the_hang_time_after_the_level_drops() {
        // 50 ms of hang is five frames.
        let mut cleanup = Cleanup::new(-40.0, 50, false);
        assert!(cleanup.process(&mut tone(0.3), 1.0, None).open);

        for frame in 0..5 {
            assert!(
                cleanup.process(&mut tone(0.0001), 1.0, None).open,
                "frame {frame} is inside the hang time"
            );
        }
        assert!(!cleanup.process(&mut tone(0.0001), 1.0, None).open, "and then it closes");

        // Speech re-opens it immediately, with no ramp.
        assert!(cleanup.process(&mut tone(0.3), 1.0, None).open);
    }

    #[test]
    fn gain_is_applied_before_anything_else_and_cannot_clip_into_distortion() {
        let mut cleanup = Cleanup::new(-40.0, 0, false);
        let mut frame = tone(0.5);
        let outcome = cleanup.process(&mut frame, 4.0, None);
        assert!(outcome.level <= 1.0, "clamped, not wrapped");
        assert!(outcome.level > 0.99, "and it did reach full scale");
        assert!(frame.iter().all(|s| s.abs() <= 1.0));
    }

    #[test]
    fn push_to_talk_overrides_the_gate_in_both_directions() {
        let mut cleanup = Cleanup::new(-40.0, 0, false);
        // Held, and silent: transmitted anyway. Somebody holding the key meant to be heard, and
        // second-guessing them is how a push-to-talk key drops the first word.
        assert!(cleanup.process(&mut tone(0.0), 1.0, Some(true)).open);
        // Not held, and shouting: not transmitted.
        assert!(!cleanup.process(&mut tone(0.9), 1.0, Some(false)).open);
    }

    /// The scale trap, pinned. RNNoise works on 16-bit-scaled floats; handing it ±1.0 samples
    /// produces silence and no error at all.
    #[test]
    fn suppression_leaves_a_real_voice_audible() {
        let mut cleanup = Cleanup::new(-60.0, 0, true);
        assert!(cleanup.suppressing());

        // RNNoise needs a few frames to settle before it passes anything; feeding it a tone and
        // checking the last frame is what tells "working" apart from "scaled wrong", because a
        // scaling mistake produces zeroes forever rather than for the first few frames.
        let mut level = 0.0;
        for _ in 0..12 {
            let mut frame = tone(0.4);
            level = cleanup.process(&mut frame, 1.0, None).level;
        }
        assert!(level > 0.02, "a 0.4 tone came out at {level} — the scale conversion is wrong");
    }

    #[test]
    fn suppression_can_be_switched_off_and_on_without_allocating_twice() {
        let mut cleanup = Cleanup::new(-40.0, 0, true);
        let first = cleanup.denoiser.as_ref().map(|d| std::ptr::from_ref(&**d));
        cleanup.set_suppression(false);
        assert!(!cleanup.suppressing());
        cleanup.set_suppression(true);
        assert!(cleanup.suppressing());
        let second = cleanup.denoiser.as_ref().map(|d| std::ptr::from_ref(&**d));
        assert_eq!(first, second, "the state should be kept, not rebuilt on an audio thread");
    }

    #[test]
    fn the_threshold_and_hang_time_can_be_changed_mid_call() {
        let mut cleanup = Cleanup::new(-40.0, 0, false);
        assert!(!cleanup.process(&mut tone(0.002), 1.0, None).open);
        cleanup.set_threshold(-70.0);
        assert!(cleanup.process(&mut tone(0.002), 1.0, None).open);

        cleanup.set_hang(100);
        assert_eq!(cleanup.gate.hang_frames, 10);
    }
}
