//! The live loop against fakes, on a paused clock. Frames carry their speech
//! probability in their samples (see `ScriptedVad`), sent in real time.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tsetsen_core::ports::{Playback, SpeechToText, TextToSpeech, Thinker, Transcript};
use tsetsen_core::{Cancellation, Layer};
use tsetsen_engine::{ConversationEngine, SessionConfig, TurnOutcome, Unobserved, run_session};
use tsetsen_policy::EgressPolicy;
use tsetsen_router::{Route, Router};
use tsetsen_testkit::{FakePlayback, FakeStt, FakeThinker, FakeTts, ScriptedVad, local};

const FRAME: usize = 512;
const FRAME_TIME: Duration = Duration::from_millis(32);

fn router<P: ?Sized + tsetsen_core::Describe>(layer: Layer, provider: Arc<P>) -> Router<P> {
    let route = Route::new(provider.descriptor().id.clone());
    Router::new(layer, &route, [provider], Arc::new(EgressPolicy::new())).unwrap()
}

/// Hears "first", "second", … in order.
fn stt() -> Arc<dyn SpeechToText> {
    let calls = AtomicUsize::new(0);
    Arc::new(FakeStt::new(local("parakeet", Layer::Stt), move |_, _| {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        Ok(Transcript::new(["first", "second", "third"][n.min(2)]))
    }))
}

fn engine(reply: &[&str], playback: Arc<FakePlayback>) -> ConversationEngine {
    let thinker: Arc<dyn Thinker> = Arc::new(FakeThinker::streaming(
        local("qwen", Layer::Think),
        reply,
        Duration::from_millis(20),
    ));
    let tts: Arc<dyn TextToSpeech> = Arc::new(FakeTts::new(
        local("supertonic", Layer::Speak),
        Duration::from_millis(20),
    ));
    ConversationEngine::new(
        router(Layer::Stt, stt()),
        router(Layer::Think, thinker),
        router(Layer::Speak, tts),
        playback as Arc<dyn Playback>,
    )
}

/// Sends `(probability, duration)` runs as real-time frames, then closes.
fn microphone(runs: Vec<(f32, Duration)>) -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        for (probability, duration) in runs {
            let frames = duration.as_millis() / FRAME_TIME.as_millis();
            for _ in 0..frames {
                tokio::time::sleep(FRAME_TIME).await;
                if tx.send(vec![probability; FRAME]).await.is_err() {
                    return;
                }
            }
        }
    });
    rx
}

async fn session(engine: &ConversationEngine, runs: Vec<(f32, Duration)>) -> Vec<TurnOutcome> {
    let turns = Mutex::new(Vec::new());
    run_session(
        engine,
        &mut ScriptedVad { frame_len: FRAME },
        microphone(runs),
        &Unobserved,
        SessionConfig::default(),
        &Cancellation::new(),
        |result| turns.lock().unwrap().push(result.as_ref().unwrap().clone()),
    )
    .await
    .unwrap();
    turns.into_inner().unwrap()
}

fn spoken(outcome: &TurnOutcome) -> (&str, bool) {
    match outcome {
        TurnOutcome::Replied(reply) | TurnOutcome::Announced(reply) => {
            (reply.spoken.as_str(), reply.interrupted)
        }
        TurnOutcome::NothingHeard { .. } => panic!("nothing heard"),
    }
}

const SPEAK: f32 = 0.9;
const QUIET: f32 = 0.0;

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

#[tokio::test(start_paused = true)]
async fn hears_a_turn_and_replies_aloud() {
    let playback = Arc::new(FakePlayback::new());
    let engine = engine(&["Hello there. ", "How can I help?"], Arc::clone(&playback));
    // Speak 1 s, then stay quiet long enough for the whole reply.
    let turns = session(
        &engine,
        vec![(QUIET, ms(300)), (SPEAK, ms(1000)), (QUIET, ms(5000))],
    )
    .await;
    assert_eq!(turns.len(), 1);
    assert_eq!(spoken(&turns[0]), ("Hello there. How can I help?", false));
    assert_eq!(playback.played().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn talking_over_the_reply_stops_it_and_starts_the_next_turn() {
    let playback = Arc::new(FakePlayback::new());
    // A long first clause: ~5 s of audio.
    let engine = engine(
        &[
            "This is a deliberately long reply that keeps on going for quite some time. ",
            "More.",
        ],
        Arc::clone(&playback),
    );
    let turns = session(
        &engine,
        vec![
            (SPEAK, ms(1000)),
            (QUIET, ms(2000)), // turn ends after 650 ms; reply starts playing
            (SPEAK, ms(800)),  // barge-in after 350 ms of confident speech
            (QUIET, ms(8000)),
        ],
    )
    .await;

    assert_eq!(turns.len(), 2, "{turns:?}");
    let (first, interrupted) = spoken(&turns[0]);
    assert!(interrupted, "first reply should be interrupted");
    assert!(
        !first.is_empty() && first.len() < 40,
        "heard only the start: {first:?}"
    );
    let TurnOutcome::Replied(second) = &turns[1] else {
        panic!()
    };
    assert_eq!(second.heard, "second");
    assert!(!second.interrupted);
    assert!(!playback.played()[0].1.completed);
}

#[tokio::test(start_paused = true)]
async fn quiet_echo_does_not_interrupt() {
    let playback = Arc::new(FakePlayback::new());
    let engine = engine(
        &["A reply that plays for a few seconds without interruption."],
        Arc::clone(&playback),
    );
    let turns = session(
        &engine,
        vec![
            (SPEAK, ms(1000)),
            (QUIET, ms(1500)),
            (0.6, ms(1500)), // residual echo: above the listening bar, below barge-in
            (QUIET, ms(5000)),
        ],
    )
    .await;
    assert_eq!(turns.len(), 1);
    assert!(!spoken(&turns[0]).1, "echo interrupted the reply");
}

#[tokio::test(start_paused = true)]
async fn a_reply_in_progress_is_stopped_when_the_session_ends() {
    let playback = Arc::new(FakePlayback::new());
    let engine = engine(
        &["A long reply that is still playing when the microphone closes."],
        playback,
    );
    // The microphone closes 1 s after the turn ends, mid-reply.
    let turns = session(&engine, vec![(SPEAK, ms(1000)), (QUIET, ms(1700))]).await;
    assert_eq!(turns.len(), 1);
    assert!(spoken(&turns[0]).1);
}
