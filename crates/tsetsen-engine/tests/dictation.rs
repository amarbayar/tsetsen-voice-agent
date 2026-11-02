//! End-to-end dictation behavior against fakes: no OS, no models.

use std::sync::Arc;
use std::time::Duration;

use tsetsen_core::ports::{Anchor, Rewriter, SpeechToText, SttHints, UiAction};
use tsetsen_core::{
    Ctx, DesktopError, Interrupted, LanguageTag, Layer, ProviderError, RequestId, Sensitivity,
};
use tsetsen_engine::{
    DictationEngine, DictationError, DictationOutcome, DictationRequest, HoldReason, Provenance,
    RewriteStatus,
};
use tsetsen_policy::EgressPolicy;
use tsetsen_router::{Route, Router};
use tsetsen_testkit::{FakeDesktop, FakeRewriter, FakeStt, local, one_second_of_audio, remote};

const SPOKEN: &str = "um let's meet at 3 no wait 4pm";
const CLEAN: &str = "Let's meet at 4pm.";

fn request() -> DictationRequest {
    DictationRequest {
        anchor: Anchor {
            app: "com.tinyspeck.slackmacgap".into(),
            window: "w-1".into(),
            element: Some("e-42".into()),
        },
        audio: one_second_of_audio(),
        hints: SttHints::default(),
    }
}

fn cx() -> Ctx {
    Ctx::new(RequestId::new(7))
}

fn stt_router(
    route: &Route,
    providers: Vec<Arc<dyn SpeechToText>>,
    egress: &Arc<EgressPolicy>,
) -> Router<dyn SpeechToText> {
    Router::new(Layer::Stt, route, providers, Arc::clone(egress)).unwrap()
}

fn rewrite_router(
    route: &Route,
    providers: Vec<Arc<dyn Rewriter>>,
    egress: &Arc<EgressPolicy>,
) -> Router<dyn Rewriter> {
    Router::new(Layer::Rewrite, route, providers, Arc::clone(egress)).unwrap()
}

/// Local STT hearing `SPOKEN`, plus `rewriter` as the only cleanup provider.
fn engine_with(
    rewriter: Arc<dyn Rewriter>,
    egress: EgressPolicy,
    desktop: Arc<FakeDesktop>,
) -> DictationEngine {
    let egress = Arc::new(egress);
    let id = rewriter.descriptor().id.clone();
    let stt: Arc<dyn SpeechToText> =
        Arc::new(FakeStt::hearing(local("parakeet", Layer::Stt), SPOKEN));
    DictationEngine::new(
        stt_router(&Route::new("parakeet"), vec![stt], &egress),
        desktop,
    )
    .with_rewriter(rewrite_router(&Route::new(id), vec![rewriter], &egress))
}

fn local_cleanup() -> Arc<dyn Rewriter> {
    Arc::new(FakeRewriter::returning(
        local("qwen", Layer::Rewrite),
        CLEAN,
    ))
}

#[tokio::test]
async fn inserts_cleaned_text_into_the_anchored_field() {
    let desktop = Arc::new(FakeDesktop::verified());
    let engine = engine_with(local_cleanup(), EgressPolicy::new(), Arc::clone(&desktop));

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert_eq!(
        outcome,
        DictationOutcome::Inserted {
            text: CLEAN.into(),
            verified: true,
            provenance: Provenance {
                stt: "parakeet".into(),
                rewrite: RewriteStatus::Applied("qwen".into()),
            },
        }
    );
    assert_eq!(
        desktop.actions(),
        [(
            "w-1".into(),
            UiAction::InsertText {
                element: "e-42".into(),
                text: CLEAN.into()
            }
        )]
    );
}

#[tokio::test]
async fn remote_cleanup_is_skipped_without_a_grant() {
    let cloud = Arc::new(FakeRewriter::returning(
        remote("frontier", Layer::Rewrite, "api.example.com"),
        CLEAN,
    ));
    let engine = engine_with(
        Arc::clone(&cloud) as Arc<dyn Rewriter>,
        EgressPolicy::new(),
        Arc::new(FakeDesktop::verified()),
    );

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert!(
        cloud.requests().is_empty(),
        "transcript must not leave the device"
    );
    assert!(matches!(
        outcome,
        DictationOutcome::Inserted { text, provenance: Provenance { rewrite: RewriteStatus::NoEligibleProvider, .. }, .. }
            if text == SPOKEN
    ));
}

#[tokio::test]
async fn remote_cleanup_is_used_once_granted() {
    let mut egress = EgressPolicy::new();
    egress
        .grant_remote("frontier".into(), Sensitivity::Personal)
        .unwrap();
    let cloud = Arc::new(FakeRewriter::returning(
        remote("frontier", Layer::Rewrite, "api.example.com"),
        CLEAN,
    ));
    let engine = engine_with(
        Arc::clone(&cloud) as Arc<dyn Rewriter>,
        egress,
        Arc::new(FakeDesktop::verified()),
    );

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert!(matches!(outcome, DictationOutcome::Inserted { text, .. } if text == CLEAN));
    let sent = cloud.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].transcript, SPOKEN);
}

#[tokio::test(start_paused = true)]
async fn slow_cleanup_falls_back_to_the_raw_transcript() {
    let slow = Arc::new(
        FakeRewriter::returning(local("qwen", Layer::Rewrite), CLEAN)
            .with_delay(Duration::from_secs(5)),
    );
    let engine = engine_with(slow, EgressPolicy::new(), Arc::new(FakeDesktop::verified()));

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert!(matches!(
        outcome,
        DictationOutcome::Inserted { text, provenance: Provenance { rewrite: RewriteStatus::TimedOut(_), .. }, .. }
            if text == SPOKEN
    ));
}

#[tokio::test]
async fn an_adapter_reporting_its_deadline_counts_as_timed_out() {
    let late = Arc::new(FakeRewriter::new(local("qwen", Layer::Rewrite), |_| {
        Err(ProviderError::Interrupted(Interrupted::DeadlineExceeded))
    }));
    let engine = engine_with(late, EgressPolicy::new(), Arc::new(FakeDesktop::verified()));

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert!(matches!(
        outcome,
        DictationOutcome::Inserted { text, provenance: Provenance { rewrite: RewriteStatus::TimedOut(_), .. }, .. }
            if text == SPOKEN
    ));
}

#[tokio::test]
async fn cleanup_may_respell_words_as_the_users_vocabulary() {
    let fixer = Arc::new(FakeRewriter::returning(
        local("qwen", Layer::Rewrite),
        "Ask Amar about Tsetsen.",
    ));
    let stt = Arc::new(FakeStt::hearing(
        local("parakeet", Layer::Stt),
        "ask amor about tseten",
    ));
    let egress = Arc::new(EgressPolicy::new());
    let engine = DictationEngine::new(
        stt_router(&Route::new("parakeet"), vec![stt], &egress),
        Arc::new(FakeDesktop::verified()),
    )
    .with_rewriter(rewrite_router(&Route::new("qwen"), vec![fixer], &egress));
    let mut request = request();
    request.hints.vocabulary = vec!["Amar".into(), "Tsetsen".into()];

    let outcome = engine.run(&request, &cx()).await.unwrap();

    assert!(matches!(
        outcome,
        DictationOutcome::Inserted { text, provenance: Provenance { rewrite: RewriteStatus::Applied(_), .. }, .. }
            if text == "Ask Amar about Tsetsen."
    ));
}

#[tokio::test]
async fn guard_blocks_a_rewriter_that_answers_instead_of_transcribing() {
    let chatty = Arc::new(FakeRewriter::returning(
        local("qwen", Layer::Rewrite),
        "Sure! Here is a summary of your schedule.",
    ));
    let engine = engine_with(
        chatty,
        EgressPolicy::new(),
        Arc::new(FakeDesktop::verified()),
    );

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert!(matches!(
        outcome,
        DictationOutcome::Inserted { text, provenance: Provenance { rewrite: RewriteStatus::Rejected { .. }, .. }, .. }
            if text == SPOKEN
    ));
}

#[tokio::test]
async fn failing_cleanup_falls_back_to_the_raw_transcript() {
    let broken = Arc::new(FakeRewriter::new(local("qwen", Layer::Rewrite), |_| {
        Err(ProviderError::Unavailable("worker crashed".into()))
    }));
    let engine = engine_with(
        broken,
        EgressPolicy::new(),
        Arc::new(FakeDesktop::verified()),
    );

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert!(matches!(
        outcome,
        DictationOutcome::Inserted { text, provenance: Provenance { rewrite: RewriteStatus::Failed(_), .. }, .. }
            if text == SPOKEN
    ));
}

#[tokio::test]
async fn stt_falls_back_when_the_primary_is_unavailable() {
    let egress = Arc::new(EgressPolicy::new());
    let primary = Arc::new(FakeStt::failing(local("parakeet", Layer::Stt), || {
        ProviderError::Unavailable("not loaded".into())
    }));
    let fallback = Arc::new(FakeStt::hearing(local("whisper", Layer::Stt), SPOKEN));
    let route = Route::new("parakeet").with_fallbacks(["whisper"]);
    let engine = DictationEngine::new(
        stt_router(
            &route,
            vec![Arc::clone(&primary) as _, Arc::clone(&fallback) as _],
            &egress,
        ),
        Arc::new(FakeDesktop::verified()),
    );

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert_eq!((primary.calls(), fallback.calls()), (1, 1));
    assert!(matches!(
        outcome,
        DictationOutcome::Inserted { provenance: Provenance { stt, rewrite: RewriteStatus::Disabled }, .. }
            if stt.as_str() == "whisper"
    ));
}

#[tokio::test]
async fn stt_does_not_fall_back_on_a_rejected_request() {
    let egress = Arc::new(EgressPolicy::new());
    let primary = Arc::new(FakeStt::failing(local("parakeet", Layer::Stt), || {
        ProviderError::Rejected("clip too long".into())
    }));
    let fallback = Arc::new(FakeStt::hearing(local("whisper", Layer::Stt), SPOKEN));
    let route = Route::new("parakeet").with_fallbacks(["whisper"]);
    let engine = DictationEngine::new(
        stt_router(
            &route,
            vec![Arc::clone(&primary) as _, Arc::clone(&fallback) as _],
            &egress,
        ),
        Arc::new(FakeDesktop::verified()),
    );

    let error = engine.run(&request(), &cx()).await.unwrap_err();

    assert!(matches!(error, DictationError::SttFailed { failures } if failures.len() == 1));
    assert_eq!(fallback.calls(), 0);
}

#[tokio::test]
async fn mongolian_speech_routes_past_an_english_only_model() {
    let egress = Arc::new(EgressPolicy::new());
    let english_only = Arc::new(FakeStt::hearing(
        local("parakeet", Layer::Stt).with_languages([LanguageTag::from("en")]),
        "wrong model",
    ));
    let multilingual = Arc::new(FakeStt::hearing(
        local("whisper", Layer::Stt),
        "сайн байна уу",
    ));
    let route = Route::new("parakeet").with_fallbacks(["whisper"]);
    let engine = DictationEngine::new(
        stt_router(
            &route,
            vec![Arc::clone(&english_only) as _, multilingual as _],
            &egress,
        ),
        Arc::new(FakeDesktop::verified()),
    );
    let mut request = request();
    request.hints.language = Some("mn".into());

    let outcome = engine.run(&request, &cx()).await.unwrap();

    assert_eq!(english_only.calls(), 0);
    assert!(matches!(outcome, DictationOutcome::Inserted { text, .. } if text == "сайн байна уу"));
}

#[tokio::test]
async fn audio_never_goes_to_a_remote_stt_without_a_grant() {
    let egress = Arc::new(EgressPolicy::new());
    let cloud = Arc::new(FakeStt::hearing(
        remote("cloud-asr", Layer::Stt, "api.example.com"),
        SPOKEN,
    ));
    let engine = DictationEngine::new(
        stt_router(
            &Route::new("cloud-asr"),
            vec![Arc::clone(&cloud) as _],
            &egress,
        ),
        Arc::new(FakeDesktop::verified()),
    );

    let error = engine.run(&request(), &cx()).await.unwrap_err();

    assert_eq!(cloud.calls(), 0);
    assert!(matches!(error, DictationError::NoSttProvider { excluded } if excluded.len() == 1));
}

#[tokio::test]
async fn text_is_held_when_the_target_window_is_gone() {
    let desktop = Arc::new(FakeDesktop::new(|window, _| {
        Err(DesktopError::WindowNotFound(window.clone()))
    }));
    let engine = engine_with(local_cleanup(), EgressPolicy::new(), desktop);

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert!(matches!(
        outcome,
        DictationOutcome::Held { text, reason: HoldReason::TargetGone, .. } if text == CLEAN
    ));
}

#[tokio::test]
async fn text_is_held_when_the_user_was_not_in_a_text_field() {
    let desktop = Arc::new(FakeDesktop::verified());
    let engine = engine_with(local_cleanup(), EgressPolicy::new(), Arc::clone(&desktop));
    let mut request = request();
    request.anchor.element = None;

    let outcome = engine.run(&request, &cx()).await.unwrap();

    assert_eq!(
        desktop.actions(),
        [] as [(tsetsen_core::WindowId, tsetsen_core::ports::UiAction); 0]
    );
    assert!(matches!(
        outcome,
        DictationOutcome::Held {
            reason: HoldReason::NoTargetElement,
            ..
        }
    ));
}

#[tokio::test]
async fn silence_inserts_nothing() {
    let egress = Arc::new(EgressPolicy::new());
    let desktop = Arc::new(FakeDesktop::verified());
    let stt: Arc<dyn SpeechToText> =
        Arc::new(FakeStt::hearing(local("parakeet", Layer::Stt), "   "));
    let engine = DictationEngine::new(
        stt_router(&Route::new("parakeet"), vec![stt], &egress),
        Arc::clone(&desktop) as _,
    );

    let outcome = engine.run(&request(), &cx()).await.unwrap();

    assert_eq!(
        outcome,
        DictationOutcome::NothingHeard {
            stt: "parakeet".into()
        }
    );
    assert_eq!(
        desktop.actions(),
        [] as [(tsetsen_core::WindowId, tsetsen_core::ports::UiAction); 0]
    );
}

#[tokio::test]
async fn a_cancelled_request_does_nothing() {
    let desktop = Arc::new(FakeDesktop::verified());
    let engine = engine_with(local_cleanup(), EgressPolicy::new(), Arc::clone(&desktop));
    let cx = cx();
    cx.cancellation().cancel();

    let error = engine.run(&request(), &cx).await.unwrap_err();

    assert!(matches!(
        error,
        DictationError::Interrupted(Interrupted::Cancelled)
    ));
    assert_eq!(
        desktop.actions(),
        [] as [(tsetsen_core::WindowId, tsetsen_core::ports::UiAction); 0]
    );
}
