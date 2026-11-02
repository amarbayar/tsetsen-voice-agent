//! The flow-matching start: standard normal noise.
//!
//! Seeded per request, so the same text renders the same audio. That makes
//! tests and evals reproducible and costs nothing in naturalness.

/// `SplitMix64`: tiny, fast, and statistically sound for this purpose.
pub(crate) struct Noise(u64);

impl Noise {
    pub(crate) fn seeded(text: &str) -> Self {
        // FNV-1a of the text.
        let seed = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `(0, 1]`.
    #[allow(clippy::cast_precision_loss)] // 53 bits fit an f64 mantissa exactly.
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    /// `n` samples of the standard normal distribution (Box-Muller).
    #[allow(clippy::cast_possible_truncation)] // Noise needs no f64 precision.
    pub(crate) fn normal(&mut self, n: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(n + 1);
        while out.len() < n {
            let radius = (-2.0 * self.uniform().ln()).sqrt();
            let angle = std::f64::consts::TAU * self.uniform();
            out.push((radius * angle.cos()) as f32);
            out.push((radius * angle.sin()) as f32);
        }
        out.truncate(n);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_standard_normal_and_reproducible() {
        let samples = Noise::seeded("hello").normal(100_000);
        let mean = samples.iter().map(|&x| f64::from(x)).sum::<f64>() / 100_000.0;
        let var = samples
            .iter()
            .map(|&x| (f64::from(x) - mean).powi(2))
            .sum::<f64>()
            / 100_000.0;
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((var - 1.0).abs() < 0.03, "variance {var}");
        assert_eq!(samples[..8], Noise::seeded("hello").normal(8)[..]);
        assert_ne!(samples[..8], Noise::seeded("world").normal(8)[..]);
    }
}
