//! Against the real Silero model, with real synthesized speech (macOS `say`).
//!
//! ```sh
//! cargo test -p tsetsen-vad-silero --test live -- --ignored
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use tsetsen_core::ports::VoiceActivity;
use tsetsen_vad_silero::{FRAME_LEN, SileroVad};

fn vad() -> SileroVad {
    let path = std::env::var_os("TSETSEN_SILERO_PATH").map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".tsetsen/models/silero-vad/silero_vad.onnx")
        },
        PathBuf::from,
    );
    SileroVad::load(&path).unwrap()
}

/// Rendered once: tests run in parallel and must not race on the file.
fn speech() -> Vec<f32> {
    static SPEECH: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    SPEECH.get_or_init(render_speech).clone()
}

fn render_speech() -> Vec<f32> {
    let path = std::env::temp_dir().join("tsetsen-vad-speech.wav");
    let status = Command::new("say")
        .args(["-v", "Samantha", "-o"])
        .arg(&path)
        .args([
            "--file-format=WAVE",
            "--data-format=LEF32@16000",
            "Hello there, can you hear me talking?",
        ])
        .status()
        .expect("`say` is available (macOS)");
    assert!(status.success());
    hound::WavReader::open(&path)
        .unwrap()
        .samples::<f32>()
        .map(Result::unwrap)
        .collect()
}

fn probabilities(vad: &mut SileroVad, samples: &[f32]) -> Vec<f32> {
    samples
        .as_chunks::<FRAME_LEN>()
        .0
        .iter()
        .map(|f| vad.speech_probability(f).unwrap())
        .collect()
}

#[test]
#[ignore = "needs the Silero model and macOS `say`; see module docs"]
fn tells_speech_from_silence_and_noise() {
    let mut vad = vad();
    let silence = probabilities(&mut vad, &vec![0.0; 16_000]);
    assert!(silence.iter().all(|&p| p < 0.1), "{silence:?}");

    // Deterministic white noise at a moderate level.
    let mut x = 1u32;
    let noise: Vec<f32> = (0..16_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            #[allow(clippy::cast_precision_loss)]
            let u = x as f32 / u32::MAX as f32;
            (u - 0.5) * 0.1
        })
        .collect();
    vad.reset();
    let scores = probabilities(&mut vad, &noise);
    let false_alarms = scores.iter().filter(|&&p| p > 0.5).count();
    assert!(false_alarms * 10 < scores.len(), "{scores:?}");

    vad.reset();
    let spoken = probabilities(&mut vad, &speech());
    let speech_frames = spoken.iter().filter(|&&p| p > 0.5).count();
    eprintln!("{speech_frames}/{} speech frames", spoken.len());
    assert!(speech_frames * 2 > spoken.len(), "{spoken:?}");
}

#[test]
#[ignore = "needs the Silero model; see module docs"]
fn is_fast_enough_to_run_on_every_frame() {
    let mut vad = vad();
    let audio = speech();
    let started = Instant::now();
    let frames = probabilities(&mut vad, &audio).len();
    let per_frame = started.elapsed() / u32::try_from(frames).unwrap();
    eprintln!("{per_frame:?} per 32 ms frame");
    assert!(per_frame.as_millis() < 2, "{per_frame:?}");
}

#[test]
#[ignore = "needs the Silero model; see module docs"]
fn rejects_wrong_frame_sizes() {
    assert!(vad().speech_probability(&[0.0; 100]).is_err());
}
