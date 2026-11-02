//! Silero VAD: voice activity detection in about 0.1 ms per 32 ms frame.
//!
//! The model is recurrent. It carries state across frames and sees the last
//! 64 samples of the previous frame as context, so frames must be fed in order
//! from one continuous stream.

use std::path::Path;

use ndarray::{Array0, Array2, Array3};
use tsetsen_core::ProviderError;
use tsetsen_core::ports::VoiceActivity;
use tsetsen_onnx::ort::session::Session;
use tsetsen_onnx::ort::value::Value;
use tsetsen_onnx::{OnnxError, SessionOptions};

/// Silero runs at 16 kHz.
pub const SAMPLE_RATE_HZ: u32 = 16_000;
/// Samples per frame: 32 ms.
pub const FRAME_LEN: usize = 512;
const CONTEXT_LEN: usize = 64;
const STATE_LEN: usize = 2 * 128;

/// A loaded Silero VAD model with its stream state.
pub struct SileroVad {
    session: Session,
    state: Vec<f32>,
    context: [f32; CONTEXT_LEN],
}

impl SileroVad {
    /// Loads `silero_vad.onnx`. Fast (a few milliseconds), but blocking.
    pub fn load(model: &Path) -> Result<Self, OnnxError> {
        // One thread: frames are tiny and arrive in real time.
        let options = SessionOptions {
            intra_threads: Some(1),
            ..SessionOptions::default()
        };
        Ok(Self {
            session: tsetsen_onnx::load(model, options)?,
            state: vec![0.0; STATE_LEN],
            context: [0.0; CONTEXT_LEN],
        })
    }

    fn infer(&mut self, frame: &[f32]) -> Result<f32, String> {
        let mut input = Vec::with_capacity(CONTEXT_LEN + FRAME_LEN);
        input.extend_from_slice(&self.context);
        input.extend_from_slice(frame);
        let input = Value::from_array(
            Array2::from_shape_vec((1, CONTEXT_LEN + FRAME_LEN), input)
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let state = Value::from_array(
            Array3::from_shape_vec((2, 1, 128), self.state.clone()).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let sr = Value::from_array(Array0::from_elem((), i64::from(SAMPLE_RATE_HZ)))
            .map_err(|e| e.to_string())?;

        let out = self
            .session
            .run(tsetsen_onnx::ort::inputs!("input" => &input, "state" => &state, "sr" => &sr))
            .map_err(|e| e.to_string())?;
        let (_, probability) = out["output"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        let probability = probability.first().copied().ok_or("no output")?;
        let (_, state) = out["stateN"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        if state.len() != STATE_LEN {
            return Err(format!("unexpected state size {}", state.len()));
        }
        self.state.copy_from_slice(state);
        self.context
            .copy_from_slice(&frame[FRAME_LEN - CONTEXT_LEN..]);
        Ok(probability.clamp(0.0, 1.0))
    }
}

impl VoiceActivity for SileroVad {
    fn sample_rate_hz(&self) -> u32 {
        SAMPLE_RATE_HZ
    }

    fn frame_len(&self) -> usize {
        FRAME_LEN
    }

    fn speech_probability(&mut self, frame: &[f32]) -> Result<f32, ProviderError> {
        if frame.len() != FRAME_LEN {
            return Err(ProviderError::Rejected(format!(
                "Silero needs {FRAME_LEN}-sample frames, got {}",
                frame.len()
            )));
        }
        self.infer(frame).map_err(ProviderError::Unavailable)
    }

    fn reset(&mut self) {
        self.state.fill(0.0);
        self.context.fill(0.0);
    }
}
