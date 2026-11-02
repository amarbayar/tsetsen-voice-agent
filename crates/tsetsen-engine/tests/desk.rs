//! The front desk and its workers, on a paused clock: tasks run side by
//! side, only one approval question is open at a time, and a stopped task
//! stays stopped and silent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::time::Instant;
use tsetsen_core::ports::{
    Playback, SpeechToText, TextToSpeech, ThinkEvent, ThinkRequest, ThinkStream, Thinker, Turn,
    UiAction, UiNode,
};
use tsetsen_core::{
    Ctx, Describe, ElementRef, Layer, ProviderDescriptor, ProviderError, RequestId,
};
use tsetsen_engine::{
    ActionOutcome, Conversation, ConversationEngine, DesktopTools, Reply, TurnOutcome, Unobserved,
};
use tsetsen_policy::EgressPolicy;
use tsetsen_router::{Route, Router};
use tsetsen_testkit::{
    FakeDesktop, FakePlayback, FakeStt, FakeThinker, FakeTts, local, one_second_of_audio, tool_call,
};

fn router<P: ?Sized + Describe>(layer: Layer, provider: Arc<P>) -> Router<P> {
    let route = Route::new(provider.descriptor().id.clone());
    Router::new(layer, &route, vec![provider], Arc::new(EgressPolicy::new())).unwrap()
}

fn text(t: &str) -> ThinkEvent {
    ThinkEvent::Text(t.into())
}

/// Workers whose scripts depend on their task: the first script whose key
/// is in the task's brief, one reply per step. Each step takes `pace`.
struct Workers {
    descriptor: ProviderDescriptor,
    scripts: Vec<(&'static str, Vec<Vec<ThinkEvent>>)>,
    pace: Duration,
    /// When each task's steps started, by key.
    started: Mutex<HashMap<&'static str, Vec<Instant>>>,
}

impl Workers {
    fn new(scripts: Vec<(&'static str, Vec<Vec<ThinkEvent>>)>, pace: Duration) -> Self {
        Self {
            descriptor: local("workers", Layer::Think),
            scripts,
            pace,
            started: Mutex::new(HashMap::new()),
        }
    }

    fn started(&self, key: &str) -> Vec<Instant> {
        self.started
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_default()
    }
}

impl Describe for Workers {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

#[async_trait]
impl Thinker for Workers {
    async fn think(&self, request: &ThinkRequest, _cx: &Ctx) -> Result<ThinkStream, ProviderError> {
        let Some(Turn::User { text: brief }) = request.turns.first() else {
            panic!("a task starts with its brief");
        };
        let (key, script) = self
            .scripts
            .iter()
            .find(|(key, _)| brief.contains(key))
            .unwrap_or_else(|| panic!("no script for {brief:?}"));
        let step = request
            .turns
            .iter()
            .filter(|t| matches!(t, Turn::Assistant { .. }))
            .count();
        self.started
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .push(Instant::now());
        let reply = script
            .get(step)
            .or(script.last())
            .cloned()
            .unwrap_or_default();
        let pace = self.pace;
        let stream = futures_util::stream::iter(reply).then(move |event| async move {
            tokio::time::sleep(pace).await;
            Ok(event)
        });
        Ok(Box::pin(stream))
    }
}

struct Setup {
    engine: ConversationEngine,
    front: Arc<FakeThinker>,
    workers: Arc<Workers>,
    desktop: Arc<FakeDesktop>,
}

fn setup(
    heard: &[&str],
    front: Vec<Vec<ThinkEvent>>,
    workers: Workers,
    desktop: FakeDesktop,
) -> Setup {
    let stt: Arc<dyn SpeechToText> = Arc::new(FakeStt::hearing_in_turn(
        local("parakeet", Layer::Stt),
        heard,
    ));
    let front = Arc::new(FakeThinker::scripted(local("front", Layer::Think), front));
    let workers = Arc::new(workers);
    let desktop = Arc::new(desktop);
    let engine = ConversationEngine::new(
        router(Layer::Stt, stt),
        router(Layer::Think, Arc::clone(&front) as Arc<dyn Thinker>),
        router(
            Layer::Speak,
            Arc::new(FakeTts::new(
                local("tts", Layer::Speak),
                Duration::from_millis(10),
            )) as Arc<dyn TextToSpeech>,
        ),
        Arc::new(FakePlayback::new()) as Arc<dyn Playback>,
    )
    .with_worker_thinker(router(
        Layer::Think,
        Arc::clone(&workers) as Arc<dyn Thinker>,
    ))
    .with_tools(Arc::new(DesktopTools::new(
        Arc::clone(&desktop) as Arc<dyn tsetsen_core::ports::Desktop>
    )));
    Setup {
        engine,
        front,
        workers,
        desktop,
    }
}

fn start(id: &str, what: &str) -> ThinkEvent {
    tool_call(
        id,
        "start_task",
        &serde_json::json!({ "what": what }).to_string(),
    )
}

async fn turn(s: &Setup, conversation: &mut Conversation) -> Reply {
    match s
        .engine
        .respond(
            conversation,
            &one_second_of_audio(),
            &Unobserved,
            &Ctx::new(RequestId::new(1)),
        )
        .await
        .unwrap()
    {
        TurnOutcome::Replied(reply) => reply,
        other => panic!("expected a reply, got {other:?}"),
    }
}

async fn announce(s: &Setup, conversation: &mut Conversation) -> Option<Reply> {
    s.engine
        .announce(conversation, &Unobserved, &Ctx::new(RequestId::new(2)))
        .await
        .unwrap()
}

fn tool_results(turns: &[Turn]) -> Vec<String> {
    turns
        .iter()
        .filter_map(|t| match t {
            Turn::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

fn chat(value: &str) -> UiNode {
    UiNode {
        role: "window".into(),
        children: vec![UiNode {
            element: Some(ElementRef::new("composer")),
            role: "textarea".into(),
            name: Some("Message".into()),
            value: Some(value.into()),
            states: vec!["focused".into()],
            actions: vec!["TypeText".into()],
            ..UiNode::default()
        }],
        ..UiNode::default()
    }
}

#[tokio::test(start_paused = true)]
async fn two_requests_run_side_by_side() {
    let s = setup(
        &["open safari and notes"],
        vec![
            vec![start("d1", "Open Safari."), start("d2", "Open Notes.")],
            vec![text("Both are open.")],
        ],
        Workers::new(
            vec![
                (
                    "Safari",
                    vec![
                        vec![tool_call("s", "open_app", r#"{"app":"Safari"}"#)],
                        vec![text("Safari is open.")],
                    ],
                ),
                (
                    "Notes",
                    vec![
                        vec![tool_call("n", "open_app", r#"{"app":"Notes"}"#)],
                        vec![text("Notes is open.")],
                    ],
                ),
            ],
            Duration::from_secs(1),
        ),
        FakeDesktop::verified().with_installed(&["Safari", "Notes"]),
    );
    let began = Instant::now();
    let reply = turn(&s, &mut Conversation::default()).await;
    let mut opened = s.desktop.opened();
    opened.sort();
    assert_eq!(opened, ["Notes", "Safari"]);
    let (safari, notes) = (s.workers.started("Safari"), s.workers.started("Notes"));
    assert_eq!(
        safari[0], notes[0],
        "both tasks start at once, not one after the other"
    );
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "two seconds of work each, done together: {:?}",
        began.elapsed()
    );
    let told = tool_results(&s.front.requests()[1].turns);
    assert!(
        told[0].starts_with("t1 finished") && told[1].starts_with("t2 finished"),
        "{told:?}"
    );
    assert_eq!(reply.spoken, "Both are open.");
}

#[tokio::test(start_paused = true)]
async fn only_one_question_is_open_at_a_time() {
    let s = setup(
        &["send both messages", "yes", "no"],
        vec![
            vec![
                start("d1", "Send the message in Slack."),
                start("d2", "Send the message in Messages."),
            ],
            vec![text("The Slack one is sent.")],
            vec![text("Okay, I won't.")],
        ],
        Workers::new(
            vec![
                (
                    "Slack",
                    vec![
                        vec![tool_call(
                            "s",
                            "press_keys",
                            r#"{"keys":"return","app":"Slack"}"#,
                        )],
                        vec![text("Sent in Slack.")],
                    ],
                ),
                (
                    "Messages",
                    vec![vec![tool_call(
                        "m",
                        "press_keys",
                        r#"{"keys":"return","app":"Messages"}"#,
                    )]],
                ),
            ],
            Duration::from_millis(10),
        ),
        FakeDesktop::verified()
            .with_window("Slack", "general", chat("See you at 5"))
            .with_window("Messages", "Mary", chat("Love you")),
    );
    let mut conversation = Conversation::default();

    // Both are held; only the first is asked about.
    let first = turn(&s, &mut conversation).await;
    assert!(
        first
            .spoken
            .ends_with("I'll press return in Slack. Shall I go ahead?"),
        "{}",
        first.spoken
    );
    assert!(
        !first.spoken.contains("Messages"),
        "one question at a time: {}",
        first.spoken
    );
    assert_eq!(
        s.engine.awaiting_approval().as_deref(),
        Some("press return in Slack")
    );
    assert!(!s.engine.has_news(), "nothing interrupts an open question");

    // Yes approves the question asked, and only that one.
    let second = turn(&s, &mut conversation).await;
    assert_eq!(second.actions[0].outcome, ActionOutcome::Done);
    assert_eq!(s.desktop.actions().len(), 1);
    assert!(matches!(
        &s.desktop.actions()[0].1,
        UiAction::PressKeys { .. }
    ));

    // Then the other is asked about in the same breath, read back word for
    // word by code, not by another model step.
    assert_eq!(
        second.spoken,
        "The Slack one is sent. I'll press return in Messages. Shall I go ahead?"
    );
    assert_eq!(
        s.engine.awaiting_approval().as_deref(),
        Some("press return in Messages")
    );
    assert_eq!(s.front.requests().len(), 2, "no model step to ask");
    assert!(!s.engine.has_news());

    let third = turn(&s, &mut conversation).await;
    assert_eq!(third.actions[0].outcome, ActionOutcome::Declined);
    assert_eq!(s.desktop.actions().len(), 1, "Messages was not sent");
    assert_eq!(s.engine.awaiting_approval(), None);
}

#[tokio::test(start_paused = true)]
async fn a_stopped_task_does_nothing_more_and_says_nothing() {
    let s = setup(
        &["open safari", "never mind"],
        vec![
            vec![start("d1", "Open Safari.")],
            vec![text("Working on it.")],
            vec![tool_call("d2", "stop_task", r#"{"task":"t1"}"#)],
            vec![text("Okay, I've stopped.")],
        ],
        Workers::new(
            vec![(
                "Safari",
                vec![
                    vec![tool_call("s", "open_app", r#"{"app":"Safari"}"#)],
                    vec![text("Safari is open.")],
                ],
            )],
            // Slower than the desk waits.
            Duration::from_secs(20),
        ),
        FakeDesktop::verified().with_installed(&["Safari"]),
    );
    let mut conversation = Conversation::default();
    let first = turn(&s, &mut conversation).await;
    assert_eq!(first.spoken, "Working on it.");
    assert!(tool_results(&s.front.requests()[1].turns)[0].contains("carries on in the background"));
    let second = turn(&s, &mut conversation).await;
    assert_eq!(second.spoken, "Okay, I've stopped.");

    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(s.desktop.opened().is_empty(), "stopped before it acted");
    assert!(!s.engine.has_news(), "and it stays quiet");
}

#[tokio::test(start_paused = true)]
async fn news_from_the_background_is_spoken_when_it_arrives() {
    let s = setup(
        &["send it to mary"],
        vec![
            vec![start("d1", "Send the message in Messages.")],
            vec![text("I'm on it.")],
        ],
        Workers::new(
            vec![(
                "Messages",
                vec![vec![tool_call(
                    "m",
                    "press_keys",
                    r#"{"keys":"return","app":"Messages"}"#,
                )]],
            )],
            // Slower than the desk waits.
            Duration::from_secs(20),
        ),
        FakeDesktop::verified().with_window("Messages", "Mary", chat("Love you")),
    );
    let mut conversation = Conversation::default();
    let first = turn(&s, &mut conversation).await;
    assert_eq!(first.spoken, "I'm on it.");
    assert!(!s.engine.has_news());

    let mut news = s.engine.news().expect("a desk");
    tokio::time::timeout(Duration::from_secs(60), news.changed())
        .await
        .expect("the task reports")
        .unwrap();
    assert!(s.engine.has_news());
    // Only an approval: the read-back is the whole announcement.
    let asked = announce(&s, &mut conversation).await.unwrap();
    assert_eq!(
        asked.spoken,
        "I'll press return in Messages. Shall I go ahead?"
    );
    assert_eq!(s.front.requests().len(), 2, "no model step to ask");
    assert_eq!(
        s.engine.awaiting_approval().as_deref(),
        Some("press return in Messages")
    );
    assert!(s.desktop.actions().is_empty(), "nothing sent before the OK");
}

#[tokio::test(start_paused = true)]
async fn a_promise_the_front_does_not_keep_is_kept_by_code_and_never_said() {
    let s = setup(
        &["open safari for me"],
        vec![
            // Ignores the forced choice: a decision, never heard.
            vec![text("Of course. Let me open it for you.")],
            vec![text("Sure. I'll open it right now.")],
            vec![text("Safari is open.")],
        ],
        Workers::new(
            vec![(
                "safari",
                vec![
                    vec![tool_call("s", "open_app", r#"{"app":"Safari"}"#)],
                    vec![text("Safari is open.")],
                ],
            )],
            Duration::from_millis(10),
        ),
        FakeDesktop::verified().with_installed(&["Safari"]),
    );
    let reply = turn(&s, &mut Conversation::default()).await;
    assert_eq!(s.desktop.opened(), ["Safari"], "handed on with her words");
    assert_eq!(reply.spoken, "Sure. Safari is open.");
    let requests = s.front.requests();
    assert!(requests[0].must_use_tool && !requests[1].must_use_tool);
}

#[tokio::test(start_paused = true)]
async fn an_answer_the_front_does_not_pass_on_goes_to_the_task_that_asked() {
    let s = setup(
        &["open my notes", "the shopping one"],
        vec![
            vec![start("d1", "Open my notes.")],
            vec![text("Which note would you like?")],
            vec![text("I'll open that one.")],
            vec![text("I'll open it now.")],
            vec![text("It's open.")],
        ],
        Workers::new(
            vec![(
                "notes",
                vec![
                    vec![text("Which note would you like?")],
                    vec![tool_call("n", "open_app", r#"{"app":"Notes"}"#)],
                    vec![text("The shopping note is open.")],
                ],
            )],
            Duration::from_millis(10),
        ),
        FakeDesktop::verified().with_installed(&["Notes"]),
    );
    let mut conversation = Conversation::default();
    let first = turn(&s, &mut conversation).await;
    assert_eq!(first.spoken, "Which note would you like?");
    let second = turn(&s, &mut conversation).await;
    assert_eq!(s.desktop.opened(), ["Notes"]);
    assert_eq!(s.workers.started("notes").len(), 3, "one task, continued");
    assert_eq!(second.spoken, "It's open.");
}

#[tokio::test(start_paused = true)]
async fn a_task_that_insists_it_sent_what_never_ran_is_not_believed() {
    let s = setup(
        &["send it to mary"],
        vec![
            vec![start("d1", "Send the message in Messages.")],
            vec![text("It hasn't gone yet, I'm sorry.")],
        ],
        Workers::new(
            vec![(
                "Messages",
                vec![
                    vec![text("I've sent it!")],
                    vec![text("Done, it was sent.")],
                ],
            )],
            Duration::from_millis(10),
        ),
        FakeDesktop::verified().with_window("Messages", "Mary", chat("Love you")),
    );
    let reply = turn(&s, &mut Conversation::default()).await;
    let told = tool_results(&s.front.requests()[1].turns);
    assert!(
        told[0].starts_with("t1 could not finish: it has not been done yet"),
        "{told:?}"
    );
    assert!(
        !told[0].contains("sent"),
        "the false claim is not passed on: {told:?}"
    );
    assert!(s.desktop.actions().is_empty());
    assert_eq!(reply.spoken, "It hasn't gone yet, I'm sorry.");
}
