//! Against the real Supertonic model. Speech is checked by transcribing it
//! back with the real Parakeet model, so "it sounds right" is measured, not
//! assumed.
//!
//! ```sh
//! cargo test --release -p tsetsen-tts-supertonic --test live -- --ignored --test-threads=1
//! ```
//!
//! `TSETSEN_SUPERTONIC_DIR` and `TSETSEN_PARAKEET_DIR` override model locations.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tsetsen_core::ports::{AudioClip, SpeechRequest, SpeechToText, SttHints, TextToSpeech};
use tsetsen_core::{Ctx, Interrupted, ProviderError, RequestId};
use tsetsen_stt_parakeet::{ParakeetConfig, ParakeetStt};
use tsetsen_tts_supertonic::{SupertonicConfig, SupertonicTts};

fn model_dir(var: &str, name: &str) -> PathBuf {
    std::env::var_os(var).map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").expect("HOME is set"))
                .join(".tsetsen/models")
                .join(name)
        },
        PathBuf::from,
    )
}

fn tts() -> Arc<SupertonicTts> {
    static TTS: OnceLock<Arc<SupertonicTts>> = OnceLock::new();
    Arc::clone(TTS.get_or_init(|| {
        let started = Instant::now();
        let dir = model_dir("TSETSEN_SUPERTONIC_DIR", "supertonic-3");
        let tts = SupertonicTts::load(&SupertonicConfig::new("supertonic", dir)).unwrap();
        eprintln!("supertonic loaded in {:?}", started.elapsed());
        Arc::new(tts)
    }))
}

fn stt() -> Arc<ParakeetStt> {
    static STT: OnceLock<Arc<ParakeetStt>> = OnceLock::new();
    Arc::clone(STT.get_or_init(|| {
        let dir = model_dir("TSETSEN_PARAKEET_DIR", "parakeet-tdt-0.6b-v3");
        Arc::new(ParakeetStt::load(&ParakeetConfig::new("parakeet", dir)).unwrap())
    }))
}

fn cx() -> Ctx {
    Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(60))
}

async fn say(text: &str) -> AudioClip {
    let request = SpeechRequest {
        text: text.into(),
        ..SpeechRequest::default()
    };
    tts().synthesize(&request, &cx()).await.unwrap()
}

/// Averages each output sample's span of input samples: a box low-pass plus
/// decimation, good enough to feed speech recognition in a test.
fn to_16k(clip: &AudioClip) -> AudioClip {
    let ratio = f64::from(clip.sample_rate_hz()) / 16_000.0;
    let input = clip.samples();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let out: Vec<f32> = (0..(input.len() as f64 / ratio) as usize)
        .map(|i| {
            let (start, end) = (
                (i as f64 * ratio) as usize,
                (((i + 1) as f64 * ratio) as usize).min(input.len()),
            );
            input[start..end].iter().sum::<f32>() / (end - start).max(1) as f32
        })
        .collect();
    AudioClip::new(16_000, out).unwrap()
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[tokio::test]
#[ignore = "needs the Supertonic and Parakeet models; see module docs"]
async fn speech_is_intelligible_to_speech_recognition() {
    for text in [
        "Sure, I've sent the report to Sarah.",
        "Your next meeting starts at 3:30, in room 14.",
        "I couldn't find that file. Do you want me to search your downloads?",
    ] {
        let audio = say(text).await;
        let heard = stt()
            .transcribe(&to_16k(&audio), &SttHints::default(), &cx())
            .await
            .unwrap()
            .text;
        eprintln!("{:?} -> {heard:?}", audio.duration());
        let (said, heard) = (words(text), words(&heard));
        let found = said.iter().filter(|w| heard.contains(w)).count();
        assert!(
            found * 10 >= said.len() * 8,
            "only {found}/{} words: {heard:?}",
            said.len()
        );
    }
}

#[tokio::test]
#[ignore = "needs the Supertonic model; see module docs"]
async fn renders_a_clause_much_faster_than_real_time() {
    let clause = "Okay, I'll move your standup to eleven.";
    say(clause).await; // warm up
    let started = Instant::now();
    let audio = say(clause).await;
    let took = started.elapsed();
    let rtf = took.as_secs_f64() / audio.duration().as_secs_f64();
    eprintln!(
        "{:?} of speech in {took:?} (real-time factor {rtf:.3})",
        audio.duration()
    );
    assert_eq!(audio.sample_rate_hz(), 44_100);
    assert!(audio.duration() > Duration::from_secs(1));
    assert!(rtf < 0.3, "real-time factor {rtf:.3}");
}

#[tokio::test]
#[ignore = "needs the Supertonic model; see module docs"]
async fn same_text_renders_the_same_audio() {
    assert_eq!(say("Deterministic.").await, say("Deterministic.").await);
}

#[tokio::test]
#[ignore = "needs the Supertonic model; see module docs"]
async fn empty_and_unknown_inputs_are_handled() {
    assert_eq!(say("   ").await.samples().len(), 0);
    let mn = SpeechRequest {
        text: "Сайн байна уу".into(),
        language: Some("mn".into()),
        ..SpeechRequest::default()
    };
    let error = tts().synthesize(&mn, &cx()).await.unwrap_err();
    assert!(
        matches!(error, ProviderError::Unsupported(_)) && error.allows_fallback(),
        "{error:?}"
    );
    let voice = SpeechRequest {
        text: "Hi.".into(),
        voice: Some("nobody".into()),
        ..SpeechRequest::default()
    };
    assert!(matches!(
        tts().synthesize(&voice, &cx()).await,
        Err(ProviderError::Rejected(_))
    ));
}

#[tokio::test]
#[ignore = "needs the Supertonic model; see module docs"]
async fn long_text_is_split_and_still_rendered() {
    let long = "This is a long reply that keeps going. ".repeat(12);
    let audio = say(&long).await;
    assert!(
        audio.duration() > Duration::from_secs(15),
        "{:?}",
        audio.duration()
    );
}

#[tokio::test]
#[ignore = "needs the Supertonic model; see module docs"]
async fn honors_the_request_deadline() {
    let request = SpeechRequest {
        text: "A sentence that takes a while to render. ".repeat(6),
        ..SpeechRequest::default()
    };
    let cx = Ctx::new(RequestId::new(1)).with_timeout(Duration::from_millis(5));
    let error = tts().synthesize(&request, &cx).await.unwrap_err();
    assert!(
        matches!(
            error,
            ProviderError::Interrupted(Interrupted::DeadlineExceeded)
        ),
        "{error:?}"
    );
}
