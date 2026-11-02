//! Supertonic inference: duration prediction, text encoding, flow-matching
//! denoising of a latent, and vocoding.

use std::collections::HashMap;
use std::path::Path;

use ndarray::{Array, Array1, Array2, Array3};
use serde::Deserialize;
use tsetsen_onnx::ort::session::Session;
use tsetsen_onnx::ort::value::Value;
use tsetsen_onnx::{OnnxError, SessionOptions};

use crate::noise::Noise;
use crate::text::CharIndex;

#[derive(Deserialize)]
struct Config {
    ae: AutoencoderConfig,
    ttl: LatentConfig,
}

#[derive(Deserialize)]
struct AutoencoderConfig {
    sample_rate: u32,
    base_chunk_size: usize,
}

#[derive(Deserialize)]
struct LatentConfig {
    chunk_compress_factor: usize,
    latent_dim: usize,
}

#[derive(Deserialize)]
struct StyleFile {
    style_ttl: StyleTensor,
    style_dp: StyleTensor,
}

#[derive(Deserialize)]
struct StyleTensor {
    data: Vec<Vec<Vec<f32>>>,
    dims: [usize; 3],
}

impl StyleTensor {
    fn into_array(self) -> Result<Array3<f32>, String> {
        let flat: Vec<f32> = self.data.into_iter().flatten().flatten().collect();
        Array3::from_shape_vec(self.dims, flat).map_err(|e| e.to_string())
    }
}

/// A voice: style vectors for the duration predictor and the text-to-latent model.
struct Voice {
    ttl: Array3<f32>,
    dp: Array3<f32>,
}

pub(crate) struct Model {
    sample_rate_hz: u32,
    chunk_size: usize,
    latent_channels: usize,
    chars: CharIndex,
    voices: HashMap<String, Voice>,
    duration_predictor: Session,
    text_encoder: Session,
    vector_estimator: Session,
    vocoder: Session,
}

/// What went wrong running the model.
pub(crate) type InferError = String;

fn ort_error(e: impl std::fmt::Display) -> InferError {
    e.to_string()
}

impl Model {
    pub(crate) fn load(
        dir: &Path,
        options: SessionOptions,
        acceleration: crate::Acceleration,
    ) -> Result<Self, OnnxError> {
        let bad = |path: &Path, message: String| OnnxError::Load {
            path: path.to_owned(),
            message,
        };
        let onnx = dir.join("onnx");
        let config_path = onnx.join("tts.json");
        let config: Config = std::fs::read(&config_path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
            .map_err(|e| bad(&config_path, e))?;
        let index_path = onnx.join("unicode_indexer.json");
        let chars = CharIndex::load(&index_path).map_err(|e| bad(&index_path, e))?;

        let voices_dir = dir.join("voice_styles");
        let mut voices = HashMap::new();
        let entries =
            std::fs::read_dir(&voices_dir).map_err(|e| bad(&voices_dir, e.to_string()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let file: StyleFile = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
                .map_err(|e| bad(&path, e))?;
            let voice = Voice {
                ttl: file.style_ttl.into_array().map_err(|e| bad(&path, e))?,
                dp: file.style_dp.into_array().map_err(|e| bad(&path, e))?,
            };
            voices.insert(name, voice);
        }
        if voices.is_empty() {
            return Err(bad(&voices_dir, "no voice styles found".into()));
        }

        let load = |name: &str, accelerated: bool| {
            tsetsen_onnx::load(
                &onnx.join(name),
                SessionOptions {
                    accelerated,
                    ..options
                },
            )
        };
        let (denoiser, all) = match acceleration {
            crate::Acceleration::None => (false, false),
            crate::Acceleration::Denoiser => (true, false),
            crate::Acceleration::All => (true, true),
        };
        Ok(Self {
            sample_rate_hz: config.ae.sample_rate,
            chunk_size: config.ae.base_chunk_size * config.ttl.chunk_compress_factor,
            latent_channels: config.ttl.latent_dim * config.ttl.chunk_compress_factor,
            chars,
            voices,
            duration_predictor: load("duration_predictor.onnx", all)?,
            text_encoder: load("text_encoder.onnx", all)?,
            vector_estimator: load("vector_estimator.onnx", denoiser)?,
            vocoder: load("vocoder.onnx", all)?,
        })
    }

    pub(crate) fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    pub(crate) fn has_voice(&self, voice: &str) -> bool {
        self.voices.contains_key(voice)
    }

    pub(crate) fn voice_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.voices.keys().cloned().collect();
        names.sort();
        names
    }

    /// Renders one normalized piece of text. Returns no samples for text the
    /// model has no characters for.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn render(
        &mut self,
        text: &str,
        language: &str,
        voice: &str,
        steps: usize,
        speed: f32,
    ) -> Result<Vec<f32>, InferError> {
        let voice = self.voices.get(voice).ok_or("unknown voice")?;
        let ids = self.chars.encode(text, language);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let len = ids.len();
        let text_ids = Value::from_array(Array2::from_shape_vec((1, len), ids).map_err(ort_error)?)
            .map_err(ort_error)?;
        let text_mask = Value::from_array(Array3::<f32>::ones((1, 1, len))).map_err(ort_error)?;
        let style_dp = Value::from_array(voice.dp.clone()).map_err(ort_error)?;
        let style_ttl = Value::from_array(voice.ttl.clone()).map_err(ort_error)?;

        let seconds = {
            let out = self
                .duration_predictor
                .run(tsetsen_onnx::ort::inputs!(
                    "text_ids" => &text_ids,
                    "style_dp" => &style_dp,
                    "text_mask" => &text_mask
                ))
                .map_err(ort_error)?;
            let (_, duration) = out["duration"]
                .try_extract_tensor::<f32>()
                .map_err(ort_error)?;
            duration.first().copied().ok_or("no duration")? / speed
        };
        if !seconds.is_finite() || seconds <= 0.0 {
            return Ok(Vec::new());
        }

        let text_emb = {
            let out = self
                .text_encoder
                .run(tsetsen_onnx::ort::inputs!(
                    "text_ids" => &text_ids,
                    "style_ttl" => &style_ttl,
                    "text_mask" => &text_mask
                ))
                .map_err(ort_error)?;
            let (shape, data) = out["text_emb"]
                .try_extract_tensor::<f32>()
                .map_err(ort_error)?;
            let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
            Value::from_array(Array::from_shape_vec(dims, data.to_vec()).map_err(ort_error)?)
                .map_err(ort_error)?
        };

        let samples = (seconds * self.sample_rate_hz as f32) as usize;
        let frames = samples.div_ceil(self.chunk_size).max(1);
        let mut latent = Array3::from_shape_vec(
            (1, self.latent_channels, frames),
            Noise::seeded(text).normal(self.latent_channels * frames),
        )
        .map_err(ort_error)?;
        let latent_mask =
            Value::from_array(Array3::<f32>::ones((1, 1, frames))).map_err(ort_error)?;
        let total = Value::from_array(Array1::from_elem(1, steps as f32)).map_err(ort_error)?;

        for step in 0..steps {
            let current =
                Value::from_array(Array1::from_elem(1, step as f32)).map_err(ort_error)?;
            let noisy = Value::from_array(latent).map_err(ort_error)?;
            let out = self
                .vector_estimator
                .run(tsetsen_onnx::ort::inputs!(
                    "noisy_latent" => &noisy,
                    "text_emb" => &text_emb,
                    "style_ttl" => &style_ttl,
                    "latent_mask" => &latent_mask,
                    "text_mask" => &text_mask,
                    "current_step" => &current,
                    "total_step" => &total
                ))
                .map_err(ort_error)?;
            let (shape, data) = out["denoised_latent"]
                .try_extract_tensor::<f32>()
                .map_err(ort_error)?;
            let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
            latent = Array3::from_shape_vec((dims[0], dims[1], dims[2]), data.to_vec())
                .map_err(ort_error)?;
        }

        let latent = Value::from_array(latent).map_err(ort_error)?;
        let out = self
            .vocoder
            .run(tsetsen_onnx::ort::inputs!("latent" => &latent))
            .map_err(ort_error)?;
        let (_, wav) = out["wav_tts"]
            .try_extract_tensor::<f32>()
            .map_err(ort_error)?;
        Ok(wav[..samples.min(wav.len())].to_vec())
    }
}
