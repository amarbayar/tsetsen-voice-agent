//! Against the real Parakeet model, with real synthesized speech (macOS `say`).
//!
//! ```sh
//! # model: see README ("Against real models")
//! cargo test -p tsetsen-stt-parakeet --test live -- --ignored --test-threads=1
//! ```
//!
//! `TSETSEN_PARAKEET_DIR` overrides the model directory.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tsetsen_core::ports::{AudioClip, SpeechToText, SttHints};
use tsetsen_core::{Ctx, Describe, Interrupted, LanguageTag, ProviderError, RequestId};
use tsetsen_stt_parakeet::{ParakeetConfig, ParakeetStt};

fn model_dir() -> PathBuf {
    std::env::var_os("TSETSEN_PARAKEET_DIR").map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").expect("HOME is set"))
                .join(".tsetsen/models/parakeet-tdt-0.6b-v3")
        },
        PathBuf::from,
    )
}

/// Loaded once: loading takes seconds and every test can share it.
fn stt() -> Arc<ParakeetStt> {
    static STT: OnceLock<Arc<ParakeetStt>> = OnceLock::new();
    Arc::clone(STT.get_or_init(|| {
        let started = Instant::now();
        let stt = ParakeetStt::load(&ParakeetConfig::new("parakeet", model_dir())).unwrap();
        eprintln!("model loaded in {:?}", started.elapsed());
        Arc::new(stt)
    }))
}

/// Renders `text` with a macOS voice to 16 kHz mono float samples.
fn speak(voice: &str, text: &str) -> AudioClip {
    let path = std::env::temp_dir().join(format!(
        "tsetsen-{voice}-{:x}.wav",
        text.bytes()
            .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
    ));
    let status = Command::new("say")
        .args(["-v", voice, "-o"])
        .arg(&path)
        .args(["--file-format=WAVE", "--data-format=LEF32@16000", text])
        .status()
        .expect("`say` is available (macOS)");
    assert!(status.success(), "say failed for voice {voice}");
    let mut reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().sample_rate, 16_000);
    let samples: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
    AudioClip::new(16_000, samples).unwrap()
}

fn cx() -> Ctx {
    Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(60))
}

async fn transcribe(audio: &AudioClip) -> Result<String, ProviderError> {
    stt()
        .transcribe(audio, &SttHints::default(), &cx())
        .await
        .map(|t| t.text)
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[tokio::test]
#[ignore = "needs the Parakeet model and macOS `say`; see module docs"]
async fn transcribes_speech_across_accents() {
    let text = "Please send the quarterly report to Sarah before Friday.";
    for voice in ["Samantha", "Daniel", "Karen", "Moira", "Tessa"] {
        let audio = speak(voice, text);
        let heard = transcribe(&audio).await.unwrap();
        eprintln!("{voice:>9}: {heard:?}");
        let heard = words(&heard);
        for word in ["send", "quarterly", "report", "sarah", "friday"] {
            assert!(
                heard.iter().any(|w| w == word),
                "{voice} missed {word:?}: {heard:?}"
            );
        }
    }
}

#[tokio::test]
#[ignore = "needs the Parakeet model and macOS `say`; see module docs"]
async fn is_much_faster_than_real_time() {
    let audio = speak(
        "Samantha",
        "Okay so for the launch plan we should do the beta on the first of November \
         and then the public launch three weeks later, and the docs need to be done first.",
    );
    transcribe(&audio).await.unwrap(); // warm up
    let started = Instant::now();
    transcribe(&audio).await.unwrap();
    let elapsed = started.elapsed();
    let rtf = elapsed.as_secs_f64() / audio.duration().as_secs_f64();
    eprintln!(
        "{:?} of audio in {elapsed:?} (real-time factor {rtf:.3})",
        audio.duration()
    );
    assert!(rtf < 0.2, "real-time factor {rtf:.3}");
}

#[tokio::test]
#[ignore = "needs the Parakeet model; see module docs"]
async fn silence_produces_no_words() {
    let audio = AudioClip::new(16_000, vec![0.0; 16_000 * 2]).unwrap();
    let heard = transcribe(&audio).await.unwrap();
    assert!(words(&heard).len() <= 1, "{heard:?}");
}

#[tokio::test]
#[ignore = "needs the Parakeet model; see module docs"]
async fn rejects_audio_that_is_not_16_khz() {
    let audio = AudioClip::new(44_100, vec![0.0; 44_100]).unwrap();
    let error = transcribe(&audio).await.unwrap_err();
    assert!(matches!(error, ProviderError::Unsupported(_)), "{error:?}");
    assert!(error.allows_fallback());
}

#[tokio::test]
#[ignore = "needs the Parakeet model and macOS `say`; see module docs"]
async fn honors_the_request_deadline() {
    let audio = speak(
        "Samantha",
        &"This sentence is long enough to take a while. ".repeat(8),
    );
    let cx = Ctx::new(RequestId::new(1)).with_timeout(Duration::from_millis(5));
    let error = stt()
        .transcribe(&audio, &SttHints::default(), &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ProviderError::Interrupted(Interrupted::DeadlineExceeded)
        ),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Parakeet model and macOS `say`; see module docs"]
async fn concurrent_requests_are_serialized_safely() {
    let a = speak("Samantha", "The first request.");
    let b = speak("Daniel", "The second request.");
    let (ra, rb) = tokio::join!(transcribe(&a), transcribe(&b));
    assert!(words(&ra.unwrap()).contains(&"first".to_owned()));
    assert!(words(&rb.unwrap()).contains(&"second".to_owned()));
}

#[test]
#[ignore = "needs the Parakeet model; see module docs"]
fn declares_its_languages_so_mongolian_routes_elsewhere() {
    let descriptor = stt().descriptor().clone();
    assert!(descriptor.supports_language(&LanguageTag::from("en-US")));
    assert!(descriptor.supports_language(&LanguageTag::from("de")));
    assert!(!descriptor.supports_language(&LanguageTag::from("mn")));
}
