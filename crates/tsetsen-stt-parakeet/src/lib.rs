//! NVIDIA Parakeet TDT speech-to-text, on ONNX Runtime via `parakeet-rs`.
//!
//! Parakeet TDT 0.6B v3 is fast and accurate for the 25 European languages it
//! was trained on, and declares exactly those, so the router sends anything
//! else (Mongolian, for one) to the next provider in the route.
//!
//! Inference is CPU-bound and blocking. It runs on Tokio's blocking pool, one
//! utterance at a time per loaded model.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use parakeet_rs::{ExecutionConfig, ExecutionProvider, ParakeetTDT, Transcriber};
use tsetsen_core::ports::{AudioClip, SpeechToText, SttHints, Transcript};
use tsetsen_core::{
    Ctx, Describe, Interrupted, LanguageTag, Layer, Locality, ProviderDescriptor, ProviderError,
    ProviderId,
};

/// The only sample rate Parakeet accepts. Capture resamples before this point.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Languages Parakeet TDT 0.6B v3 was trained on.
pub const V3_LANGUAGES: [&str; 25] = [
    "bg", "cs", "da", "de", "el", "en", "es", "et", "fi", "fr", "hr", "hu", "it", "lt", "lv", "mt",
    "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv", "uk",
];

/// Where inference runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Device {
    /// ONNX Runtime on the CPU.
    #[default]
    Cpu,
    /// Core ML on Apple Silicon. Requires the `coreml` feature.
    #[cfg(feature = "coreml")]
    CoreMl,
}

/// How to load a Parakeet model.
#[derive(Clone, Debug)]
pub struct ParakeetConfig {
    id: ProviderId,
    model_dir: PathBuf,
    device: Device,
    intra_threads: Option<usize>,
    languages: Vec<LanguageTag>,
}

impl ParakeetConfig {
    /// A TDT v3 model in `model_dir` (containing `encoder-model.onnx`,
    /// `decoder_joint-model.onnx` and `vocab.txt`), on the CPU.
    pub fn new(id: impl Into<ProviderId>, model_dir: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            model_dir: model_dir.into(),
            device: Device::default(),
            intra_threads: None,
            languages: V3_LANGUAGES.iter().map(|&tag| tag.into()).collect(),
        }
    }

    /// Runs inference on `device`.
    #[must_use]
    pub fn with_device(mut self, device: Device) -> Self {
        self.device = device;
        self
    }

    /// Caps ONNX Runtime's threads per inference.
    #[must_use]
    pub fn with_intra_threads(mut self, threads: usize) -> Self {
        self.intra_threads = Some(threads);
        self
    }

    /// Overrides the declared languages, e.g. for a fine-tuned model.
    #[must_use]
    pub fn with_languages(mut self, languages: impl IntoIterator<Item = LanguageTag>) -> Self {
        self.languages = languages.into_iter().collect();
        self
    }

    /// The model directory.
    #[must_use]
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }
}

/// The model could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// ONNX Runtime could not start.
    #[error("ONNX Runtime environment: {0}")]
    Runtime(String),
    /// The model files are missing or invalid.
    #[error("failed to load Parakeet model from `{dir}`: {source}")]
    Model {
        /// The model directory.
        dir: PathBuf,
        /// The underlying error.
        #[source]
        source: parakeet_rs::Error,
    },
}

/// A loaded Parakeet model, ready to transcribe.
pub struct ParakeetStt {
    descriptor: ProviderDescriptor,
    model: Arc<Mutex<ParakeetTDT>>,
}

impl ParakeetStt {
    /// Loads the model. Takes seconds and blocks; call it during startup or
    /// from a blocking task, not from an async request path.
    pub fn load(config: &ParakeetConfig) -> Result<Self, LoadError> {
        let provider = match config.device {
            Device::Cpu => ExecutionProvider::Cpu,
            #[cfg(feature = "coreml")]
            Device::CoreMl => ExecutionProvider::CoreML,
        };
        let mut execution = ExecutionConfig::new()
            .with_environment(
                tsetsen_onnx::environment().map_err(|e| LoadError::Runtime(e.to_string()))?,
            )
            .with_execution_provider(provider);
        if let Some(threads) = config.intra_threads {
            execution = execution.with_intra_threads(threads);
        }
        let model =
            ParakeetTDT::from_pretrained(&config.model_dir, Some(execution)).map_err(|source| {
                LoadError::Model {
                    dir: config.model_dir.clone(),
                    source,
                }
            })?;
        let descriptor = ProviderDescriptor::new(config.id.clone(), Layer::Stt, Locality::Local)
            .with_languages(config.languages.iter().cloned());
        Ok(Self {
            descriptor,
            model: Arc::new(Mutex::new(model)),
        })
    }
}

impl Describe for ParakeetStt {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

#[async_trait]
impl SpeechToText for ParakeetStt {
    async fn transcribe(
        &self,
        audio: &AudioClip,
        _hints: &SttHints,
        cx: &Ctx,
    ) -> Result<Transcript, ProviderError> {
        if audio.sample_rate_hz() != SAMPLE_RATE_HZ {
            return Err(ProviderError::Unsupported(format!(
                "Parakeet needs {SAMPLE_RATE_HZ} Hz audio, got {} Hz",
                audio.sample_rate_hz()
            )));
        }
        if audio.samples().is_empty() {
            return Ok(Transcript::new(""));
        }
        cx.check()?;

        let model = Arc::clone(&self.model);
        let samples = audio.samples().to_vec();
        let job = tokio::task::spawn_blocking(move || {
            model
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .transcribe_samples(samples, SAMPLE_RATE_HZ, 1, None)
        });

        // CPU inference cannot be interrupted. On deadline we stop waiting and
        // report it; the blocking task finishes in the background and its
        // result is dropped.
        let joined = match cx.remaining() {
            Some(left) => tokio::time::timeout(left, job)
                .await
                .map_err(|_| ProviderError::Interrupted(Interrupted::DeadlineExceeded))?,
            None => job.await,
        };
        let result = joined
            .map_err(|e| ProviderError::Unavailable(format!("inference task failed: {e}")))?
            .map_err(|e| ProviderError::Unavailable(format!("inference failed: {e}")))?;

        // Never log samples or text: both are the user's voice and words.
        tracing::debug!(provider = %self.descriptor.id, duration = ?audio.duration(), "transcribed");
        Ok(Transcript::new(result.text.trim()))
    }
}
