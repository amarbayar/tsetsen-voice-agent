use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::{Ctx, Describe, LanguageTag, ProviderError};

/// Mono PCM audio with 32-bit float samples in `[-1.0, 1.0]`.
///
/// Samples are reference-counted so a clip can be handed to a fallback
/// provider without copying it.
///
/// `Debug` shows the format and length only, never samples: a stray `{:?}` in
/// a log line must not leak the user's voice.
#[derive(Clone, PartialEq)]
pub struct AudioClip {
    sample_rate_hz: u32,
    samples: Arc<[f32]>,
}

impl std::fmt::Debug for AudioClip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioClip")
            .field("sample_rate_hz", &self.sample_rate_hz)
            .field("samples", &self.samples.len())
            .field("duration", &self.duration())
            .finish()
    }
}

/// An [`AudioClip`] could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("audio sample rate must be greater than zero")]
pub struct InvalidAudio;

impl AudioClip {
    /// Wraps `samples` recorded at `sample_rate_hz`.
    pub fn new(sample_rate_hz: u32, samples: impl Into<Arc<[f32]>>) -> Result<Self, InvalidAudio> {
        if sample_rate_hz == 0 {
            return Err(InvalidAudio);
        }
        Ok(Self {
            sample_rate_hz,
            samples: samples.into(),
        })
    }

    /// Samples per second.
    #[must_use]
    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// The raw samples.
    #[must_use]
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Length of the clip.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // Exact below 2^52 samples, about 3,000 years at 48 kHz.
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / f64::from(self.sample_rate_hz))
    }
}

/// Hints that improve recognition. Providers may ignore any of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SttHints {
    /// Expected language, if known.
    pub language: Option<LanguageTag>,
    /// Names and jargon the user says often.
    pub vocabulary: Vec<String>,
}

/// What was said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    /// The recognized text, unedited.
    pub text: String,
    /// The detected language, if the provider reports one.
    pub language: Option<LanguageTag>,
}

impl Transcript {
    /// A transcript with no detected language.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            language: None,
        }
    }
}

/// Speech to text.
#[async_trait]
pub trait SpeechToText: Describe + Send + Sync {
    /// Transcribes one utterance.
    async fn transcribe(
        &self,
        audio: &AudioClip,
        hints: &SttHints,
        cx: &Ctx,
    ) -> Result<Transcript, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_sample_rate() {
        assert_eq!(AudioClip::new(0, vec![0.0]), Err(InvalidAudio));
    }

    #[test]
    fn debug_output_never_contains_samples() {
        let clip = AudioClip::new(16_000, vec![0.123_45; 4]).unwrap();
        let shown = format!("{clip:?}");
        assert!(!shown.contains("0.123"), "{shown}");
        assert!(shown.contains("samples: 4"), "{shown}");
    }

    #[test]
    fn duration_follows_sample_rate() {
        let clip = AudioClip::new(16_000, vec![0.0; 8_000]).unwrap();
        assert_eq!(clip.duration(), Duration::from_millis(500));
    }
}
