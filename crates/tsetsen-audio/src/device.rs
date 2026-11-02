//! Opening the default microphone and speaker with `cpal`.
//!
//! Callbacks run on the OS audio thread and only move samples: downmix or
//! upmix, convert the sample format, and push or pop a lock-free ring. All
//! real work happens on the processing thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use ringbuf::traits::{Consumer, Producer};
use ringbuf::{HeapCons, HeapProd};
use tsetsen_core::AudioError;

/// What the output callback shares with the speaker.
pub(crate) struct OutputState {
    /// Real (non-silence) samples that reached the device.
    pub(crate) consumed: AtomicU64,
    /// Set to drop everything queued; cleared by the callback once done.
    pub(crate) flush: AtomicBool,
}

/// The open streams and their formats. Must stay on the thread that opened it.
pub(crate) struct Devices {
    _input: cpal::Stream,
    _output: cpal::Stream,
    pub(crate) input_rate: u32,
    pub(crate) output_rate: u32,
    pub(crate) input_name: String,
    pub(crate) output_name: String,
}

fn device_error(e: impl std::fmt::Display) -> AudioError {
    AudioError::Device(e.to_string())
}

fn name(device: &cpal::Device) -> String {
    device
        .description()
        .map_or_else(|_| "unknown device".to_owned(), |d| d.name().to_owned())
}

/// Opens and starts both streams.
pub(crate) fn open(
    capture: HeapProd<f32>,
    playback: HeapCons<f32>,
    rendered: HeapProd<f32>,
    output: Arc<OutputState>,
) -> Result<Devices, AudioError> {
    let host = cpal::default_host();
    let mic = host
        .default_input_device()
        .ok_or_else(|| AudioError::NoDevice("no microphone".into()))?;
    let speaker = host
        .default_output_device()
        .ok_or_else(|| AudioError::NoDevice("no speaker".into()))?;
    let in_config = mic.default_input_config().map_err(device_error)?;
    let out_config = speaker.default_output_config().map_err(device_error)?;

    let input = match in_config.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&mic, &in_config.config(), capture),
        SampleFormat::I16 => build_input::<i16>(&mic, &in_config.config(), capture),
        SampleFormat::I32 => build_input::<i32>(&mic, &in_config.config(), capture),
        other => Err(AudioError::Device(format!(
            "unsupported microphone format {other}"
        ))),
    }?;
    let output_stream = match out_config.sample_format() {
        SampleFormat::F32 => {
            build_output::<f32>(&speaker, &out_config.config(), playback, rendered, output)
        }
        SampleFormat::I16 => {
            build_output::<i16>(&speaker, &out_config.config(), playback, rendered, output)
        }
        SampleFormat::I32 => {
            build_output::<i32>(&speaker, &out_config.config(), playback, rendered, output)
        }
        other => Err(AudioError::Device(format!(
            "unsupported speaker format {other}"
        ))),
    }?;
    input.play().map_err(permission_or_device)?;
    output_stream.play().map_err(device_error)?;
    Ok(Devices {
        _input: input,
        _output: output_stream,
        input_rate: in_config.sample_rate(),
        output_rate: out_config.sample_rate(),
        input_name: name(&mic),
        output_name: name(&speaker),
    })
}

fn permission_or_device(e: impl std::fmt::Display) -> AudioError {
    let message = e.to_string();
    if message.to_lowercase().contains("permission") {
        AudioError::PermissionDenied(message)
    } else {
        AudioError::Device(message)
    }
}

fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut capture: HeapProd<f32>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels.max(1));
    #[allow(clippy::cast_precision_loss)]
    let scale = 1.0 / channels as f32;
    device
        .build_input_stream(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for frame in data.chunks_exact(channels) {
                    let mono: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() * scale;
                    // A full ring means the processing thread stalled; drop
                    // rather than block the OS audio thread.
                    let _ = capture.try_push(mono);
                }
            },
            |error| tracing::warn!(%error, "microphone stream error"),
            None,
        )
        .map_err(permission_or_device)
}

fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut playback: HeapCons<f32>,
    mut rendered: HeapProd<f32>,
    state: Arc<OutputState>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels.max(1));
    device
        .build_output_stream(
            *config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                if state.flush.load(Ordering::Acquire) {
                    playback.clear();
                    state.flush.store(false, Ordering::Release);
                }
                let mut real = 0u64;
                for frame in data.chunks_exact_mut(channels) {
                    let sample = playback.try_pop().inspect(|_| real += 1).unwrap_or(0.0);
                    frame.fill(T::from_sample(sample));
                    // The echo reference: exactly what the speaker played.
                    let _ = rendered.try_push(sample);
                }
                state.consumed.fetch_add(real, Ordering::AcqRel);
            },
            |error| tracing::warn!(%error, "speaker stream error"),
            None,
        )
        .map_err(device_error)
}
