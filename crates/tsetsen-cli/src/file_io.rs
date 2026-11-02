//! A WAV file as the microphone and another as the speaker, in real time.
//!
//! Runs the exact live pipeline (VAD, turn-taking, barge-in) without audio
//! hardware, for testing and for reproducing a conversation.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tsetsen_audio::{PIPELINE_RATE_HZ, Resampler};
use tsetsen_core::ports::{AudioClip, Playback, Played};
use tsetsen_core::{AudioError, Ctx};

const CHUNK: usize = 512;

/// Reads a WAV file as mono f32 at `rate_hz`.
pub(crate) fn read_wav(path: &Path, rate_hz: u32) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            #[allow(clippy::cast_precision_loss)]
            let scale = 1.0 / (1u64 << (spec.bits_per_sample.max(1) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| {
                    s.map(|s| {
                        #[allow(clippy::cast_precision_loss)]
                        let s = s as f32;
                        s * scale
                    })
                })
                .collect::<Result<_, _>>()
        }
    }
    .map_err(|e| e.to_string())?;
    #[allow(clippy::cast_precision_loss)]
    let mono: Vec<f32> = samples
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();
    Ok(Resampler::convert(spec.sample_rate, rate_hz, &mono))
}

/// Streams `samples`, then silence, as a real-time microphone. Stops when
/// the receiver is dropped.
pub(crate) fn microphone(samples: Vec<f32>) -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        #[allow(clippy::cast_precision_loss)]
        let period = Duration::from_secs_f64(CHUNK as f64 / f64::from(PIPELINE_RATE_HZ));
        let mut tick = tokio::time::interval(period);
        let mut chunks = samples.chunks(CHUNK);
        loop {
            tick.tick().await;
            let mut chunk = chunks.next().map(<[f32]>::to_vec).unwrap_or_default();
            chunk.resize(CHUNK, 0.0);
            if tx.send(chunk).await.is_err() {
                return;
            }
        }
    });
    rx
}

/// A speaker that records what it plays, in real time.
pub(crate) struct Recorder {
    rate_hz: u32,
    recorded: Mutex<Vec<f32>>,
}

impl Recorder {
    pub(crate) fn new(rate_hz: u32) -> Arc<Self> {
        Arc::new(Self {
            rate_hz,
            recorded: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn save(&self, path: &Path) -> Result<(), String> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: self.rate_hz,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
        for &s in self
            .recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            writer.write_sample(s).map_err(|e| e.to_string())?;
        }
        writer.finalize().map_err(|e| e.to_string())
    }
}

#[async_trait]
impl Playback for Recorder {
    async fn play(&self, clip: &AudioClip, cx: &Ctx) -> Result<Played, AudioError> {
        let samples = Resampler::convert(clip.sample_rate_hz(), self.rate_hz, clip.samples());
        let step = (self.rate_hz / 100) as usize; // 10 ms
        let mut played = 0;
        while played < samples.len() {
            if cx.check().is_err() {
                break;
            }
            let end = (played + step).min(samples.len());
            self.recorded
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(&samples[played..end]);
            played = end;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        #[allow(clippy::cast_precision_loss)]
        let duration = Duration::from_secs_f64(played as f64 / f64::from(self.rate_hz));
        Ok(Played {
            duration,
            completed: played >= samples.len(),
        })
    }
}
