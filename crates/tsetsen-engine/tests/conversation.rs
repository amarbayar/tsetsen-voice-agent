//! Conversation behavior against fakes, on a paused clock: streaming,
//! interruption, history, fallback and failure.

use std::sync::Arc;
use std::time::Duration;

use tsetsen_core::ports::{Playback, SpeechToText, TextToSpeech, Thinker, Transcript, Turn};
use tsetsen_core::{Cancellation, Ctx, Layer, ProviderError, ProviderId, RequestId};
use tsetsen_engine::{
    Conversation, ConversationEngine, ConversationError, TurnOutcome, Unobserved,
    VOICE_INSTRUCTIONS,
};
use tsetsen_policy::EgressPolicy;
use tsetsen_router::{Route, Router};
use tsetsen_testkit::{
    FAKE_SPEECH_PER_CHAR, FakePlayback, FakeStt, FakeThinker, FakeTts, local, one_second_of_audio,
    remote,
};

const REPLY: [&str; 5] = [
    "Sure, I can ",
    "help with that, ",
    "and it won't ",
    "take long. ",
    "Done!",
];

fn router<P: ?Sized + tsetsen_core::Describe>(layer: Layer, providers: Vec<Arc<P>>) -> Router<P> {
    let ids: Vec<_> = providers
        .iter()
        .map(|p| p.descriptor().id.clone())
        .collect();
    let route = Route::new(ids[0].clone()).with_fallbacks(ids[1..].iter().cloned());
    Router::new(layer, &route, providers, Arc::new(EgressPolicy::new())).unwrap()
}

struct Setup {
    engine: ConversationEngine,
    thinker: Arc<FakeThinker>,
    tts: Arc<FakeTts>,
    playback: Arc<FakePlayback>,
}

fn setup_with(thinkers: &[Arc<FakeThinker>], talkers: &[Arc<FakeTts>], heard: &str) -> Setup {
    let stt: Arc<dyn SpeechToText> =
        Arc::new(FakeStt::hearing(local("parakeet", Layer::Stt), heard));
    let playback = Arc::new(FakePlayback::new());
    let engine = ConversationEngine::new(
        router(Layer::Stt, vec![stt]),
        router(
            Layer::Think,
            thinkers
                .iter()
                .map(|t| Arc::clone(t) as Arc<dyn Thinker>)
                .collect(),
        ),
        router(
            Layer::Speak,
            talkers
                .iter()
                .map(|t| Arc::clone(t) as Arc<dyn TextToSpeech>)
                .collect(),
        ),
        Arc::clone(&playback) as Arc<dyn Playback>,
    );
    Setup {
        engine,
        thinker: Arc::clone(&thinkers[0]),
        tts: Arc::clone(talkers.last().unwrap()),
        playback,
    }
}

fn setup(pace: Duration) -> Setup {
    setup_with(
        &[Arc::new(FakeThinker::streaming(
            local("qwen", Layer::Think),
            &REPLY,
            pace,
        ))],
        &[Arc::new(FakeTts::new(
            local("supertonic", Layer::Speak),
            Duration::from_millis(30),
        ))],
        "can you help me",
    )
}

fn cx() -> Ctx {
    Ctx::new(RequestId::new(1))
}

async fn respond(setup: &Setup, conversation: &mut Conversation, cx: &Ctx) -> TurnOutcome {
    setup
        .engine
        .respond(conversation, &one_second_of_audio(), &Unobserved, cx)
        .await
        .unwrap()
}

fn reply(outcome: TurnOutcome) -> tsetsen_engine::Reply {
    match outcome {
        TurnOutcome::Replied(reply) => reply,
        other @ (TurnOutcome::NothingHeard { .. } | TurnOutcome::Announced(_)) => {
            panic!("expected a reply, got {other:?}")
        }
    }
}

#[tokio::test(start_paused = true)]
async fn speaks_the_reply_clause_by_clause_and_records_it() {
    let s = setup(Duration::from_millis(40));
    let mut conversation = Conversation::default();
    let reply = reply(respond(&s, &mut conversation, &cx()).await);

    assert_eq!(
        s.tts.texts(),
        [
            "Sure, I can help with that,",
            "and it won't take long.",
            "Done!"
        ]
    );
    assert!(s.playback.played().iter().all(|(_, p)| p.completed));
    assert_eq!(
        reply.written,
        "Sure, I can help with that, and it won't take long. Done!"
    );
    assert_eq!(reply.spoken, reply.written);
    assert!(!reply.interrupted && reply.degraded.is_none());
    assert_eq!(
        conversation.turns(),
        [
            Turn::user("can you help me"),
            Turn::assistant(reply.spoken.as_str())
        ]
    );
    let sent = &s.thinker.requests()[0];
    assert_eq!(sent.instructions, VOICE_INSTRUCTIONS);
    assert_eq!(sent.turns, [Turn::user("can you help me")]);
}

#[tokio::test(start_paused = true)]
async fn audio_starts_long_before_the_thinker_finishes() {
    let s = setup(Duration::from_millis(200)); // 5 fragments: 1 s of thinking
    let reply = reply(respond(&s, &mut Conversation::default(), &cx()).await);
    let first = reply.timing.first_audio.unwrap();
    // First clause after 2 fragments (400 ms) plus 30 ms of rendering.
    assert_eq!(first, Duration::from_millis(430));
    // Total: the last clause starts playing only after the thinker is done
    // and the earlier clauses have played.
    assert!(reply.timing.finished > Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn interruption_keeps_only_what_the_user_heard() {
    let s = setup(Duration::ZERO);
    let mut conversation = Conversation::default();
    let cancel = Cancellation::new();
    let cx = cx().with_cancellation(cancel.clone());
    // First clause: 27 chars = 1.35 s of audio. Interrupt 1 s into it.
    let interrupt = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(30 + 1000)).await;
            cancel.cancel();
        }
    });
    let reply = reply(respond(&s, &mut conversation, &cx).await);
    interrupt.await.unwrap();

    assert!(reply.interrupted);
    // 1.0 of 1.35 s heard: 74% of 6 words, rounded down to 4.
    assert_eq!(reply.spoken, "Sure, I can help");
    assert_eq!(conversation.turns()[1], Turn::assistant("Sure, I can help"));
    let played = s.playback.played();
    assert_eq!(played.len(), 1);
    assert!(!played[0].1.completed);
    assert_eq!(played[0].0, FAKE_SPEECH_PER_CHAR * 27);
}

#[tokio::test(start_paused = true)]
async fn interruption_before_any_audio_keeps_the_user_turn() {
    let s = setup(Duration::from_secs(1));
    let mut conversation = Conversation::default();
    let cancel = Cancellation::new();
    let cx = cx().with_cancellation(cancel.clone());
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancel.cancel();
        }
    });
    let reply = reply(respond(&s, &mut conversation, &cx).await);
    assert!(reply.interrupted);
    assert_eq!(reply.spoken, "");
    assert_eq!(reply.timing.first_audio, None);
    assert_eq!(conversation.turns(), [Turn::user("can you help me")]);
    assert!(s.playback.played().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_next_turn_carries_the_history() {
    let s = setup(Duration::ZERO);
    let mut conversation = Conversation::default();
    respond(&s, &mut conversation, &cx()).await;
    respond(&s, &mut conversation, &cx()).await;
    let second = &s.thinker.requests()[1];
    assert_eq!(second.turns.len(), 3);
    assert_eq!(second.turns[2], Turn::user("can you help me"));
}

#[tokio::test(start_paused = true)]
async fn falls_back_to_the_next_thinker_and_talker() {
    let broken = Arc::new(FakeThinker::failing(local("cloud", Layer::Think), || {
        ProviderError::Unavailable("down".into())
    }));
    let working = Arc::new(FakeThinker::streaming(
        local("qwen", Layer::Think),
        &["Hi there."],
        Duration::ZERO,
    ));
    let s = setup_with(
        &[broken, Arc::clone(&working)],
        &[
            Arc::new(FakeTts::failing(local("pocket", Layer::Speak))),
            Arc::new(FakeTts::new(
                local("supertonic", Layer::Speak),
                Duration::ZERO,
            )),
        ],
        "hello",
    );
    let reply = reply(respond(&s, &mut Conversation::default(), &cx()).await);
    assert_eq!(reply.thinker.as_ref().map(ProviderId::as_str), Some("qwen"));
    assert_eq!(
        reply.talker.as_ref().map(ProviderId::as_str),
        Some("supertonic")
    );
    assert_eq!(reply.spoken, "Hi there.");
}

#[tokio::test(start_paused = true)]
async fn a_thinker_failing_mid_reply_keeps_what_was_spoken() {
    let s = setup_with(
        &[Arc::new(FakeThinker::failing_after(
            local("qwen", Layer::Think),
            &["The first part is fine. ", "And then"],
        ))],
        &[Arc::new(FakeTts::new(
            local("supertonic", Layer::Speak),
            Duration::ZERO,
        ))],
        "tell me",
    );
    let mut conversation = Conversation::default();
    let reply = reply(respond(&s, &mut conversation, &cx()).await);
    assert_eq!(reply.spoken, "The first part is fine.");
    assert!(reply.degraded.as_deref().unwrap().contains("mid-reply"));
    assert!(!reply.interrupted);
    assert_eq!(conversation.turns().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn silence_is_not_a_turn() {
    let s = setup_with(
        &[Arc::new(FakeThinker::streaming(
            local("qwen", Layer::Think),
            &["Hi."],
            Duration::ZERO,
        ))],
        &[Arc::new(FakeTts::new(
            local("supertonic", Layer::Speak),
            Duration::ZERO,
        ))],
        "  ",
    );
    let mut conversation = Conversation::default();
    assert!(matches!(
        respond(&s, &mut conversation, &cx()).await,
        TurnOutcome::NothingHeard { .. }
    ));
    assert!(conversation.turns().is_empty());
    assert!(s.thinker.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_conversation_never_leaves_the_device_without_a_grant() {
    let stt: Arc<dyn SpeechToText> =
        Arc::new(FakeStt::new(local("parakeet", Layer::Stt), |_, _| {
            Ok(Transcript::new("hello"))
        }));
    let cloud: Arc<dyn Thinker> = Arc::new(FakeThinker::streaming(
        remote("frontier", Layer::Think, "api.example.com"),
        &["Hi."],
        Duration::ZERO,
    ));
    let tts: Arc<dyn TextToSpeech> = Arc::new(FakeTts::new(
        local("supertonic", Layer::Speak),
        Duration::ZERO,
    ));
    let engine = ConversationEngine::new(
        router(Layer::Stt, vec![stt]),
        router(Layer::Think, vec![cloud]),
        router(Layer::Speak, vec![tts]),
        Arc::new(FakePlayback::new()),
    );
    let result = engine
        .respond(
            &mut Conversation::default(),
            &one_second_of_audio(),
            &Unobserved,
            &cx(),
        )
        .await;
    assert!(
        matches!(result, Err(ConversationError::NoThinker { .. })),
        "{result:?}"
    );
}
