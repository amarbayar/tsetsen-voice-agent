//! Microphone, speaker and echo cancellation, on every desktop OS.
//!
//! Everything runs at the hardware's native rate and format. A processing
//! thread converts to 16 kHz mono and runs WebRTC's AEC3 (via `sonora`, a
//! pure-Rust port), using the audio that actually reached the speaker as the
//! echo reference. The microphone stream the pipeline sees is clean of the
//! assistant's own voice, which is what makes barge-in work on laptop
//! speakers.

mod device;
mod resample;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use async_trait::async_trait;
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapProd, HeapRb};
use tokio::sync::mpsc;
use tsetsen_core::ports::{AudioClip, Playback, Played};
use tsetsen_core::{AudioError, Ctx};

use crate::device::OutputState;
pub use crate::resample::Resampler;

/// The rate of the microphone stream handed to the pipeline.
pub const PIPELINE_RATE_HZ: u32 = 16_000;
/// Echo cancellation works in 10 ms frames.
const AEC_FRAME: usize = (PIPELINE_RATE_HZ / 100) as usize;

/// How to process the microphone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    /// Remove the assistant's own voice from the microphone. Needed for
    /// barge-in on speakers; harmless with headphones.
    pub echo_cancellation: bool,
    /// Suppress steady background noise.
    pub noise_suppression: bool,
    /// Samples per chunk sent to the pipeline.
    pub chunk_len: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            noise_suppression: false,
            chunk_len: 512,
        }
    }
}

/// Which devices were opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Microphone name.
    pub input: String,
    /// Microphone sample rate.
    pub input_rate_hz: u32,
    /// Speaker name.
    pub output: String,
    /// Speaker sample rate.
    pub output_rate_hz: u32,
}

/// Open audio devices. Dropping it stops them.
pub struct Audio {
    speaker: Arc<Speaker>,
    info: DeviceInfo,
    shutdown: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Audio {
    /// Opens the default microphone and speaker. Returns the devices and the
    /// cleaned microphone stream: 16 kHz mono chunks, in real time.
    pub fn open(config: AudioConfig) -> Result<(Self, mpsc::Receiver<Vec<f32>>), AudioError> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let output = Arc::new(OutputState {
            consumed: AtomicU64::new(0),
            flush: AtomicBool::new(false),
        });
        // Two seconds of headroom at any plausible device rate.
        let (capture_tx, mut capture_rx) = HeapRb::<f32>::new(192_000).split();
        let (rendered_tx, mut rendered_rx) = HeapRb::<f32>::new(192_000).split();
        let (playback_tx, playback_rx) = HeapRb::<f32>::new(192_000).split();

        // The streams live on their own thread: `cpal` streams may not be
        // moved between threads on every platform.
        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let io = std::thread::Builder::new()
            .name("tsetsen-audio-io".into())
            .spawn({
                let shutdown = Arc::clone(&shutdown);
                let output = Arc::clone(&output);
                move || match device::open(capture_tx, playback_rx, rendered_tx, output) {
                    Ok(devices) => {
                        let info = DeviceInfo {
                            input: devices.input_name.clone(),
                            input_rate_hz: devices.input_rate,
                            output: devices.output_name.clone(),
                            output_rate_hz: devices.output_rate,
                        };
                        let _ = opened_tx.send(Ok(info));
                        while !shutdown.load(Ordering::Acquire) {
                            std::thread::park_timeout(Duration::from_millis(100));
                        }
                        drop(devices);
                    }
                    Err(error) => {
                        let _ = opened_tx.send(Err(error));
                    }
                }
            });
        let io = io.map_err(|e| AudioError::Device(e.to_string()))?;
        let info = opened_rx
            .recv()
            .map_err(|_| AudioError::Device("audio thread exited".into()))??;

        let (frames_tx, frames_rx) = mpsc::channel(256);
        let dsp = std::thread::Builder::new()
            .name("tsetsen-audio-dsp".into())
            .spawn({
                let shutdown = Arc::clone(&shutdown);
                let (input_rate, output_rate) = (info.input_rate_hz, info.output_rate_hz);
                move || {
                    let mut processor = Processor::new(config, input_rate, output_rate);
                    let mut scratch = vec![0.0; 4096];
                    while !shutdown.load(Ordering::Acquire) {
                        let mut idle = true;
                        loop {
                            let n = capture_rx.pop_slice(&mut scratch);
                            if n == 0 {
                                break;
                            }
                            idle = false;
                            processor.capture(&scratch[..n]);
                        }
                        loop {
                            let n = rendered_rx.pop_slice(&mut scratch);
                            if n == 0 {
                                break;
                            }
                            processor.rendered(&scratch[..n]);
                        }
                        for chunk in processor.process() {
                            if let Err(mpsc::error::TrySendError::Closed(_)) =
                                frames_tx.try_send(chunk)
                            {
                                return;
                            }
                        }
                        if idle {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
            });
        let dsp = dsp.map_err(|e| AudioError::Device(e.to_string()))?;

        let speaker = Arc::new(Speaker {
            rate_hz: info.output_rate_hz,
            queue: Mutex::new(playback_tx),
            output,
        });
        tracing::info!(
            input = %info.input, input_rate = info.input_rate_hz,
            output = %info.output, output_rate = info.output_rate_hz,
            "audio devices open"
        );
        Ok((
            Self {
                speaker,
                info,
                shutdown,
                threads: vec![io, dsp],
            },
            frames_rx,
        ))
    }

    /// The speaker, for the pipeline.
    #[must_use]
    pub fn speaker(&self) -> Arc<dyn Playback> {
        Arc::clone(&self.speaker) as Arc<dyn Playback>
    }

    /// The devices in use.
    #[must_use]
    pub fn devices(&self) -> &DeviceInfo {
        &self.info
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// Resampling and echo cancellation, on the processing thread.
struct Processor {
    config: AudioConfig,
    apm: Option<sonora::AudioProcessing>,
    capture_resampler: Resampler,
    render_resampler: Resampler,
    near: Vec<f32>,
    far: Vec<f32>,
    out: Vec<f32>,
}

impl Processor {
    fn new(config: AudioConfig, input_rate: u32, output_rate: u32) -> Self {
        let apm = (config.echo_cancellation || config.noise_suppression).then(|| {
            let stream = sonora::StreamConfig::new(PIPELINE_RATE_HZ, 1);
            let mut apm = sonora::AudioProcessing::builder()
                .config(sonora::Config {
                    echo_canceller: config
                        .echo_cancellation
                        .then(sonora::config::EchoCanceller::default),
                    noise_suppression: config
                        .noise_suppression
                        .then(sonora::config::NoiseSuppression::default),
                    high_pass_filter: Some(sonora::config::HighPassFilter::default()),
                    ..sonora::Config::default()
                })
                .capture_config(stream)
                .render_config(stream)
                .build();
            // A typical round trip through OS buffers; AEC3 refines it.
            let _ = apm.set_stream_delay_ms(60);
            apm
        });
        Self {
            config,
            apm,
            capture_resampler: Resampler::new(input_rate, PIPELINE_RATE_HZ),
            render_resampler: Resampler::new(output_rate, PIPELINE_RATE_HZ),
            near: Vec::new(),
            far: Vec::new(),
            out: Vec::new(),
        }
    }

    fn capture(&mut self, samples: &[f32]) {
        self.capture_resampler.process(samples, &mut self.near);
    }

    fn rendered(&mut self, samples: &[f32]) {
        self.render_resampler.process(samples, &mut self.far);
    }

    /// Processes every complete 10 ms frame; returns full pipeline chunks.
    fn process(&mut self) -> Vec<Vec<f32>> {
        // The speaker and microphone clocks drift; never let the echo
        // reference run more than 300 ms ahead of the microphone.
        let max_lead = AEC_FRAME * 30;
        if self.far.len() > self.near.len() + max_lead {
            let excess = self.far.len() - self.near.len() - AEC_FRAME * 5;
            self.far.drain(..excess);
        }
        let frames = self.near.len() / AEC_FRAME;
        let mut cleaned = [0.0f32; AEC_FRAME];
        let mut discard = [0.0f32; AEC_FRAME];
        for i in 0..frames {
            let near = &self.near[i * AEC_FRAME..(i + 1) * AEC_FRAME];
            match &mut self.apm {
                Some(apm) => {
                    let far: [f32; AEC_FRAME] = if self.far.len() >= AEC_FRAME {
                        let mut far = [0.0; AEC_FRAME];
                        far.copy_from_slice(&self.far[..AEC_FRAME]);
                        self.far.drain(..AEC_FRAME);
                        far
                    } else {
                        [0.0; AEC_FRAME]
                    };
                    let _ = apm.process_render_f32(&[&far], &mut [&mut discard]);
                    if apm
                        .process_capture_f32(&[near], &mut [&mut cleaned])
                        .is_err()
                    {
                        cleaned.copy_from_slice(near);
                    }
                }
                None => cleaned.copy_from_slice(near),
            }
            self.out.extend_from_slice(&cleaned);
        }
        self.near.drain(..frames * AEC_FRAME);
        if self.apm.is_none() {
            self.far.clear();
        }
        let chunk = self.config.chunk_len.max(1);
        let mut chunks = Vec::new();
        while self.out.len() >= chunk {
            chunks.push(self.out.drain(..chunk).collect());
        }
        chunks
    }
}

/// The default speaker.
struct Speaker {
    rate_hz: u32,
    queue: Mutex<HeapProd<f32>>,
    output: Arc<OutputState>,
}

impl Speaker {
    fn consumed(&self) -> u64 {
        self.output.consumed.load(Ordering::Acquire)
    }

    #[allow(clippy::cast_precision_loss)]
    fn duration(&self, samples: u64) -> Duration {
        Duration::from_secs_f64(samples as f64 / f64::from(self.rate_hz.max(1)))
    }
}

#[async_trait]
impl Playback for Speaker {
    async fn play(&self, clip: &AudioClip, cx: &Ctx) -> Result<Played, AudioError> {
        const POLL: Duration = Duration::from_millis(10);
        let samples = if clip.sample_rate_hz() == self.rate_hz {
            clip.samples().to_vec()
        } else {
            Resampler::convert(clip.sample_rate_hz(), self.rate_hz, clip.samples())
        };
        let total = samples.len() as u64;
        let start = self.consumed();
        let mut written = 0;
        loop {
            if cx.check().is_err() {
                self.output.flush.store(true, Ordering::Release);
                // The callback clears the queue within one buffer period.
                for _ in 0..20 {
                    if !self.output.flush.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let heard = self.consumed().saturating_sub(start).min(total);
                return Ok(Played {
                    duration: self.duration(heard),
                    completed: false,
                });
            }
            if written < samples.len() {
                let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
                written += queue.push_slice(&samples[written..]);
            }
            let heard = self.consumed().saturating_sub(start);
            if written == samples.len() && heard >= total {
                return Ok(Played {
                    duration: self.duration(total),
                    completed: true,
                });
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processor_turns_any_rate_into_pipeline_chunks() {
        for echo_cancellation in [false, true] {
            let config = AudioConfig {
                echo_cancellation,
                ..AudioConfig::default()
            };
            let mut p = Processor::new(config, 48_000, 44_100);
            p.capture(&vec![0.1; 48_000]);
            p.rendered(&vec![0.0; 44_100]);
            let chunks = p.process();
            let samples: usize = chunks.iter().map(Vec::len).sum();
            assert!(chunks.iter().all(|c| c.len() == 512));
            // One second in, minus resampler lag and the partial last chunk.
            assert!((15_000..=16_000).contains(&samples), "{samples}");
        }
    }

    #[test]
    fn a_runaway_echo_reference_is_trimmed() {
        let mut p = Processor::new(AudioConfig::default(), 16_000, 16_000);
        p.rendered(&vec![0.0; 16_000]);
        p.capture(&vec![0.0; 160]);
        p.process();
        assert!(p.far.len() <= AEC_FRAME * 30, "{}", p.far.len());
    }
}
