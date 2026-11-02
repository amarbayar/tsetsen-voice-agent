//! Supertonic 3 text-to-speech, on ONNX Runtime.
//!
//! A 99M-parameter flow-matching model that reads characters directly (no
//! phonemizer) and renders 44.1 kHz speech in 31 languages, fast enough on a
//! CPU to render each clause of a reply while the previous one plays.
//!
//! Model weights: OpenRAIL-M. The upstream project was archived in
//! September 2026; this adapter is our own implementation of its inference.

mod model;
mod noise;
mod text;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use tsetsen_core::ports::{AudioClip, SpeechRequest, TextToSpeech};
use tsetsen_core::{
    Ctx, Describe, Interrupted, LanguageTag, Layer, Locality, ProviderDescriptor, ProviderError,
    ProviderId, VoiceId,
};
use tsetsen_onnx::{OnnxError, SessionOptions};

use crate::model::Model;

/// Languages Supertonic 3 was trained on.
pub const LANGUAGES: [&str; 31] = [
    "ar", "bg", "cs", "da", "de", "el", "en", "es", "et", "fi", "fr", "hi", "hr", "hu", "id", "it",
    "ja", "ko", "lt", "lv", "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv", "tr", "uk", "vi",
];

/// The model's input limit, in characters, per rendered piece.
const MAX_PIECE_CHARS: usize = 300;

/// Pause between pieces of one request that had to be split.
const PIECE_GAP_SECONDS: f32 = 0.2;

/// Which parts of the model run on the platform accelerator (Core ML on
/// macOS) instead of the CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Acceleration {
    /// Everything on the CPU: fastest to load.
    None,
    /// The flow-matching denoiser, which does most of the work.
    #[default]
    Denoiser,
    /// Every component.
    All,
}

/// How to load and run Supertonic.
#[derive(Clone, Debug)]
pub struct SupertonicConfig {
    id: ProviderId,
    model_dir: PathBuf,
    voice: VoiceId,
    default_language: LanguageTag,
    steps: usize,
    speed: f32,
    options: SessionOptions,
    acceleration: Acceleration,
}

impl SupertonicConfig {
    /// A Supertonic 3 model in `model_dir` (with `onnx/` and `voice_styles/`).
    ///
    /// Defaults: voice `F1`, English when the request names no language,
    /// 8 denoising steps, speed 1.05.
    pub fn new(id: impl Into<ProviderId>, model_dir: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            model_dir: model_dir.into(),
            voice: VoiceId::from("F1"),
            default_language: LanguageTag::from("en"),
            steps: 8,
            speed: 1.05,
            options: SessionOptions::default(),
            acceleration: Acceleration::default(),
        }
    }

    /// The voice used when a request names none.
    #[must_use]
    pub fn with_voice(mut self, voice: impl Into<VoiceId>) -> Self {
        self.voice = voice.into();
        self
    }

    /// The language assumed when a request names none.
    #[must_use]
    pub fn with_default_language(mut self, language: impl Into<LanguageTag>) -> Self {
        self.default_language = language.into();
        self
    }

    /// Denoising steps: fewer is faster, more is cleaner. At least 1.
    #[must_use]
    pub fn with_steps(mut self, steps: usize) -> Self {
        self.steps = steps.max(1);
        self
    }

    /// Speaking rate; 1.0 is the model's natural pace. Clamped to `0.5..=2.0`.
    #[must_use]
    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = if speed.is_finite() {
            speed.clamp(0.5, 2.0)
        } else {
            1.0
        };
        self
    }

    /// Which components run on the accelerator.
    #[must_use]
    pub fn with_acceleration(mut self, acceleration: Acceleration) -> Self {
        self.acceleration = acceleration;
        self
    }

    /// Caps ONNX Runtime's threads per inference.
    #[must_use]
    pub fn with_intra_threads(mut self, threads: usize) -> Self {
        self.options.intra_threads = Some(threads);
        self
    }
}

/// The model could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// Model files are missing or invalid, or the runtime failed.
    #[error(transparent)]
    Onnx(#[from] OnnxError),
    /// The configured default voice is not among the model's voices.
    #[error("voice `{voice}` not found; available: {available:?}")]
    UnknownVoice {
        /// The configured voice.
        voice: VoiceId,
        /// The voices found.
        available: Vec<String>,
    },
}

struct Settings {
    voice: VoiceId,
    default_language: LanguageTag,
    steps: usize,
    speed: f32,
}

/// A loaded Supertonic model.
pub struct SupertonicTts {
    descriptor: ProviderDescriptor,
    settings: Arc<Settings>,
    model: Arc<Mutex<Model>>,
    sample_rate_hz: u32,
}

impl SupertonicTts {
    /// Loads the model. Takes about a second and blocks; call it at startup.
    pub fn load(config: &SupertonicConfig) -> Result<Self, LoadError> {
        let model = Model::load(&config.model_dir, config.options, config.acceleration)?;
        if !model.has_voice(config.voice.as_str()) {
            return Err(LoadError::UnknownVoice {
                voice: config.voice.clone(),
                available: model.voice_names(),
            });
        }
        let descriptor = ProviderDescriptor::new(config.id.clone(), Layer::Speak, Locality::Local)
            .with_languages(LANGUAGES.iter().map(|&tag| tag.into()));
        Ok(Self {
            descriptor,
            sample_rate_hz: model.sample_rate_hz(),
            settings: Arc::new(Settings {
                voice: config.voice.clone(),
                default_language: config.default_language.clone(),
                steps: config.steps,
                speed: config.speed,
            }),
            model: Arc::new(Mutex::new(model)),
        })
    }

    /// The sample rate of rendered audio.
    #[must_use]
    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }
}

impl Describe for SupertonicTts {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

#[async_trait]
impl TextToSpeech for SupertonicTts {
    async fn synthesize(
        &self,
        request: &SpeechRequest,
        cx: &Ctx,
    ) -> Result<AudioClip, ProviderError> {
        let language = request
            .language
            .as_ref()
            .unwrap_or(&self.settings.default_language);
        if !self.descriptor.supports_language(language) {
            return Err(ProviderError::Unsupported(format!(
                "Supertonic does not speak `{language}`"
            )));
        }
        let voice = request.voice.as_ref().unwrap_or(&self.settings.voice);
        if !self
            .model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .has_voice(voice.as_str())
        {
            return Err(ProviderError::Rejected(format!("unknown voice `{voice}`")));
        }
        let text = text::normalize(&request.text);
        if text.is_empty() {
            return AudioClip::new(self.sample_rate_hz, Vec::new())
                .map_err(|e| ProviderError::InvalidResponse(e.to_string()));
        }
        cx.check()?;

        let model = Arc::clone(&self.model);
        let settings = Arc::clone(&self.settings);
        let language = language.primary().to_owned();
        let voice = voice.as_str().to_owned();
        let sample_rate_hz = self.sample_rate_hz;
        let job = tokio::task::spawn_blocking(move || {
            let mut model = model.lock().unwrap_or_else(PoisonError::into_inner);
            let mut samples = Vec::new();
            for (i, piece) in text::split(&text, MAX_PIECE_CHARS).into_iter().enumerate() {
                if i > 0 {
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        clippy::cast_precision_loss
                    )]
                    let gap = (PIECE_GAP_SECONDS * sample_rate_hz as f32) as usize;
                    samples.resize(samples.len() + gap, 0.0);
                }
                samples.extend(model.render(
                    piece,
                    &language,
                    &voice,
                    settings.steps,
                    settings.speed,
                )?);
            }
            Ok::<_, String>(samples)
        });

        // CPU inference cannot be interrupted; on deadline we stop waiting.
        let joined = match cx.remaining() {
            Some(left) => tokio::time::timeout(left, job)
                .await
                .map_err(|_| ProviderError::Interrupted(Interrupted::DeadlineExceeded))?,
            None => job.await,
        };
        let samples = joined
            .map_err(|e| ProviderError::Unavailable(format!("synthesis task failed: {e}")))?
            .map_err(|e| ProviderError::Unavailable(format!("synthesis failed: {e}")))?;
        // Never log the text: it is the user's conversation.
        let clip = AudioClip::new(sample_rate_hz, samples)
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
        tracing::debug!(provider = %self.descriptor.id, duration = ?clip.duration(), "synthesized");
        Ok(clip)
    }
}
