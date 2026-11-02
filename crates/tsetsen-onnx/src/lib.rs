//! The process-wide ONNX Runtime that local model adapters share.
//!
//! ONNX Runtime allows one environment per process, and it must outlive every
//! session created in it. Speech-to-text, text-to-speech and voice activity
//! adapters all run on it, so it lives here instead of in any one of them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use ort;
use ort::environment::Environment;
use ort::session::Session;
use ort::session::builder::{AutoDevicePolicy, GraphOptimizationLevel};

/// ONNX Runtime could not start or load a model.
#[derive(Debug, thiserror::Error)]
pub enum OnnxError {
    /// The runtime itself could not start.
    #[error("ONNX Runtime environment: {0}")]
    Runtime(String),
    /// A model file is missing or invalid.
    #[error("failed to load `{path}`: {message}")]
    Load {
        /// The model file.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
}

/// The process's ONNX Runtime environment, created on first use.
pub fn environment() -> Result<Environment, OnnxError> {
    static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();
    ENVIRONMENT
        .get_or_init(|| ort::init().build().map_err(|e| e.to_string()))
        .clone()
        .map_err(OnnxError::Runtime)
}

/// How to run one model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionOptions {
    /// Threads per inference; ONNX Runtime's default if `None`.
    pub intra_threads: Option<usize>,
    /// Use the platform's accelerator where it supports the model's
    /// operators (Core ML on macOS), the CPU for the rest. Loads slower;
    /// worth it only for models measured to run faster.
    pub accelerated: bool,
}

/// Loads the model at `path`, fully optimized, on the CPU unless accelerated.
///
/// CPU is explicit: ONNX Runtime's default policy registers Core ML on macOS,
/// which loads several times slower and helps only some models. Adapters opt
/// in per model with [`SessionOptions::accelerated`], after measuring.
pub fn load(path: &Path, options: SessionOptions) -> Result<Session, OnnxError> {
    let fail = |e: ort::Error| OnnxError::Load {
        path: path.to_owned(),
        message: e.to_string(),
    };
    let mut builder = Session::builder(&environment()?)
        .map_err(fail)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| fail(e.into()))?
        .with_auto_device(if options.accelerated {
            AutoDevicePolicy::MaxPerformance
        } else {
            AutoDevicePolicy::PreferCPU
        })
        .map_err(|e| fail(e.into()))?;
    if let Some(threads) = options.intra_threads {
        builder = builder
            .with_intra_threads(threads)
            .map_err(|e| fail(e.into()))?;
    }
    builder.commit_from_file(path).map_err(fail)
}
