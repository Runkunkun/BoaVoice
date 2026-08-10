//! Getting between the device's sample rate and the codec's.
//!
//! Opus works at 48 kHz and so does everything on the wire. A sound device works at whatever it
//! feels like: 48 kHz usually, 44.1 kHz on plenty of hardware, 16 kHz on some headsets, and 96 kHz on
//! an interface somebody bought for music. Something has to convert, and the alternative to doing it
//! here is asking the device for 48 kHz and failing when it says no — which on the machines that say
//! no would mean no voice at all.
//!
//! This is **linear interpolation**, and that is a compromise worth naming rather than hiding. A
//! proper resampler (a windowed-sinc polyphase filter) has a much flatter response and does not fold
//! high frequencies back down; linear interpolation adds a small amount of aliasing that is audible
//! as a faint edge on sibilants. It is used anyway because the alternative is a dependency and a
//! filter design for a path that mostly does nothing: the common case is 48 kHz in and 48 kHz out,
//! where [`Resampler::passthrough`] skips this code entirely. When it *is* needed, it is for speech
//! that is about to be Opus-encoded at a bitrate that discards more than the aliasing adds.

/// Convert between two rates, one sample at a time, keeping its place between calls.
///
/// Keeping the fractional position across calls is the whole reason this is a struct: a device hands
/// over a few hundred samples at a time, and restarting the interpolation at each buffer boundary
/// produces a click at every one — 100 times a second, which is a tone rather than a click.
pub struct Resampler {
    /// Input samples consumed per output sample.
    step: f64,
    /// Where we are between the previous input sample and the next.
    position: f64,
    previous: f32,
    passthrough: bool,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        let (from, to) = (from.max(1), to.max(1));
        Resampler {
            step: from as f64 / to as f64,
            position: 0.0,
            previous: 0.0,
            passthrough: from == to,
        }
    }

    /// Whether the rates match and this is doing nothing.
    pub fn passthrough(&self) -> bool {
        self.passthrough
    }

    /// Append the converted form of `input` to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        for &sample in input {
            // Emit every output sample that falls between the previous input sample and this one.
            // Upsampling produces several per input, downsampling sometimes none.
            while self.position < 1.0 {
                let t = self.position as f32;
                out.push(self.previous + (sample - self.previous) * t);
                self.position += self.step;
            }
            self.position -= 1.0;
            self.previous = sample;
        }
    }
}

/// Average interleaved frames down to one channel.
///
/// Averaging rather than taking the first channel, which is the tempting shortcut: a stereo
/// microphone with the voice mostly in one channel comes out at half volume, and an interface whose
/// first input is not the one plugged in comes out silent.
pub fn to_mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels <= 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    for frame in interleaved.chunks_exact(channels) {
        out.push(frame.iter().sum::<f32>() / channels as f32);
    }
}

/// Write one mono sample across every channel of an interleaved output frame.
///
/// Not used by playback any more — the mixer is stereo and [`super::pipeline`] writes a pair — but it
/// is the right primitive for a mono output path and is kept for the capture side's symmetry.
#[allow(dead_code)]
pub fn spread(sample: f32, frame: &mut [f32]) {
    for slot in frame.iter_mut() {
        *slot = sample;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_rates_are_a_straight_copy() {
        let mut resampler = Resampler::new(48_000, 48_000);
        assert!(resampler.passthrough());
        let mut out = Vec::new();
        resampler.process(&[1.0, 2.0, 3.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn the_output_count_follows_the_ratio() {
        for (from, to) in [(44_100, 48_000), (48_000, 44_100), (16_000, 48_000), (96_000, 48_000)] {
            let mut resampler = Resampler::new(from, to);
            let input: Vec<f32> = (0..from as usize / 10).map(|i| (i % 100) as f32 / 100.0).collect();
            let mut out = Vec::new();
            resampler.process(&input, &mut out);

            let expected = input.len() as f64 * to as f64 / from as f64;
            let drift = (out.len() as f64 - expected).abs();
            assert!(drift <= 2.0, "{from}→{to}: got {} samples, expected ~{expected}", out.len());
        }
    }

    /// The reason this keeps state: a device delivers a few hundred samples at a time, and an
    /// interpolator that restarts at each boundary clicks at every one.
    #[test]
    fn converting_in_chunks_matches_converting_all_at_once() {
        let input: Vec<f32> = (0..4_800).map(|i| (i as f32 * 0.01).sin()).collect();

        let mut whole = Vec::new();
        Resampler::new(44_100, 48_000).process(&input, &mut whole);

        let mut chunked = Vec::new();
        let mut resampler = Resampler::new(44_100, 48_000);
        for chunk in input.chunks(137) {
            resampler.process(chunk, &mut chunked);
        }

        assert_eq!(whole.len(), chunked.len());
        for (i, (a, b)) in whole.iter().zip(&chunked).enumerate() {
            assert!((a - b).abs() < 1e-6, "sample {i}: {a} vs {b}");
        }
    }

    #[test]
    fn a_constant_signal_stays_constant_through_resampling() {
        // The simplest check that the interpolation is not introducing ripple: a flat input must
        // produce a flat output, whatever the ratio.
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        resampler.process(&[0.5; 1_000], &mut out);
        // The first sample is the initial `previous` of zero ramping in; everything after is flat.
        assert!(out[10..].iter().all(|s| (s - 0.5).abs() < 1e-6), "{:?}", &out[10..20]);
    }

    #[test]
    fn stereo_is_averaged_rather_than_halved() {
        let mut out = Vec::new();
        // Voice in the left channel only: taking the first channel would be right by luck, taking
        // the second would be silence, averaging is half — and half is recoverable with gain.
        to_mono(&[1.0, 0.0, 0.5, 0.0], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.25]);

        let mut out = Vec::new();
        to_mono(&[1.0, 1.0, 1.0, 1.0], 2, &mut out);
        assert_eq!(out, vec![1.0, 1.0]);

        // Mono in, mono out, no copying about.
        let mut out = Vec::new();
        to_mono(&[0.3, 0.4], 1, &mut out);
        assert_eq!(out, vec![0.3, 0.4]);

        // A four-channel interface, and a trailing partial frame that must not be half-counted.
        let mut out = Vec::new();
        to_mono(&[1.0, 1.0, 1.0, 1.0, 0.5], 4, &mut out);
        assert_eq!(out, vec![1.0]);
    }

    #[test]
    fn one_sample_fills_every_output_channel() {
        let mut frame = [0.0; 4];
        spread(0.25, &mut frame);
        assert_eq!(frame, [0.25; 4]);
    }
}
