//! Streaming sample-rate conversion for mono speech.
//!
//! Windowed-sinc interpolation from a precomputed kernel table: 32 taps,
//! Blackman window, cutoff just below the lower Nyquist frequency, so
//! downsampling a 48 kHz microphone to 16 kHz does not alias.

use std::f64::consts::PI;

const HALF: usize = 16;
const TAPS: usize = 2 * HALF;
const PHASES: usize = 256;

/// Converts a mono stream from one sample rate to another, in chunks.
pub struct Resampler {
    step: f64,
    position: f64,
    history: Vec<f32>,
    /// `PHASES + 1` rows of `TAPS` coefficients.
    kernel: Vec<f32>,
    passthrough: bool,
}

impl Resampler {
    /// From `from_hz` to `to_hz`. Both must be non-zero.
    #[must_use]
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        let (from, to) = (f64::from(from_hz.max(1)), f64::from(to_hz.max(1)));
        let cutoff = (to / from).min(1.0) * 0.92;
        let mut kernel = Vec::with_capacity((PHASES + 1) * TAPS);
        for phase in 0..=PHASES {
            #[allow(clippy::cast_precision_loss)]
            let frac = phase as f64 / PHASES as f64;
            for tap in 0..TAPS {
                #[allow(clippy::cast_precision_loss)]
                let t = tap as f64 - (HALF as f64 - 1.0) - frac;
                let sinc = if t.abs() < 1e-9 {
                    1.0
                } else {
                    (PI * cutoff * t).sin() / (PI * cutoff * t)
                };
                #[allow(clippy::cast_precision_loss)]
                let w = 2.0 * PI * (t + HALF as f64) / TAPS as f64;
                let window = 0.42 - 0.5 * w.cos() + 0.08 * (2.0 * w).cos();
                #[allow(clippy::cast_possible_truncation)]
                kernel.push((cutoff * sinc * window) as f32);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let start = HALF as f64 - 1.0;
        Self {
            step: from / to,
            position: start,
            history: vec![0.0; HALF],
            kernel,
            passthrough: from_hz == to_hz,
        }
    }

    /// Converts `input`, appending to `output`. Output lags input by 16
    /// samples of the source rate; [`Self::flush`] releases them.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.passthrough {
            output.extend_from_slice(input);
            return;
        }
        self.history.extend_from_slice(input);
        #[allow(clippy::cast_precision_loss)]
        while self.position + (HALF as f64) < self.history.len() as f64 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let base = self.position.floor() as usize;
            let frac = self.position - self.position.floor();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let phase = (frac * PHASES as f64).round() as usize;
            let row = &self.kernel[phase * TAPS..(phase + 1) * TAPS];
            let start = base + 1 - HALF;
            let window = &self.history[start..start + TAPS];
            output.push(window.iter().zip(row).map(|(x, k)| x * k).sum());
            self.position += self.step;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let consumed = (self.position.floor() as usize).saturating_sub(HALF - 1);
        self.history.drain(..consumed);
        #[allow(clippy::cast_precision_loss)]
        {
            self.position -= consumed as f64;
        }
    }

    /// Releases the samples held back for interpolation, as at end of stream.
    pub fn flush(&mut self, output: &mut Vec<f32>) {
        self.process(&[0.0; HALF], output);
    }

    /// Converts a whole clip.
    #[must_use]
    pub fn convert(from_hz: u32, to_hz: u32, input: &[f32]) -> Vec<f32> {
        let mut resampler = Self::new(from_hz, to_hz);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let expected = (input.len() as f64 * f64::from(to_hz) / f64::from(from_hz.max(1))) as usize;
        let mut output = Vec::with_capacity(expected + TAPS);
        resampler.process(input, &mut output);
        resampler.flush(&mut output);
        output.truncate(expected);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn sine(hz: f64, rate: u32, seconds: f64) -> Vec<f32> {
        let n = (f64::from(rate) * seconds) as usize;
        #[allow(clippy::cast_possible_truncation)]
        (0..n)
            .map(|i| (2.0 * PI * hz * i as f64 / f64::from(rate)).sin() as f32 * 0.5)
            .collect()
    }

    /// Amplitude of `hz` in `signal`, by correlation (a one-bin DFT).
    #[allow(clippy::cast_precision_loss)]
    fn amplitude(signal: &[f32], hz: f64, rate: u32) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (i, &x) in signal.iter().enumerate() {
            let angle = 2.0 * PI * hz * i as f64 / f64::from(rate);
            re += f64::from(x) * angle.cos();
            im += f64::from(x) * angle.sin();
        }
        2.0 * (re * re + im * im).sqrt() / signal.len() as f64
    }

    #[test]
    fn keeps_speech_band_tones_across_common_rates() {
        for (from, to) in [
            (48_000, 16_000),
            (44_100, 16_000),
            (44_100, 48_000),
            (16_000, 48_000),
        ] {
            let out = Resampler::convert(from, to, &sine(1_000.0, from, 0.5));
            let a = amplitude(&out[200..out.len() - 200], 1_000.0, to);
            assert!((a - 0.5).abs() < 0.02, "{from}->{to}: amplitude {a}");
        }
    }

    #[test]
    fn removes_tones_above_the_new_nyquist() {
        // 12 kHz cannot exist at 16 kHz; it must not fold down to 4 kHz.
        let out = Resampler::convert(48_000, 16_000, &sine(12_000.0, 48_000, 0.5));
        let aliased = amplitude(&out[200..out.len() - 200], 4_000.0, 16_000);
        assert!(aliased < 0.005, "aliased amplitude {aliased}");
    }

    #[test]
    fn length_matches_the_ratio_and_chunking_does_not_matter() {
        let input = sine(440.0, 48_000, 1.0);
        let whole = Resampler::convert(48_000, 16_000, &input);
        assert_eq!(whole.len(), 16_000);
        let mut streamed = Vec::new();
        let mut r = Resampler::new(48_000, 16_000);
        for chunk in input.chunks(333) {
            r.process(chunk, &mut streamed);
        }
        r.flush(&mut streamed);
        streamed.truncate(16_000);
        assert_eq!(streamed, whole);
    }

    #[test]
    fn same_rate_is_untouched() {
        let input = sine(440.0, 16_000, 0.1);
        assert_eq!(Resampler::convert(16_000, 16_000, &input), input);
    }
}
