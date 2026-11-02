//! Everyday errands, the way an older user asks for them, against fakes on a
//! paused clock: who "my son" is, which pictures "last Saturday" means,
//! exactly what gets emailed after one spoken yes, and that the same verbs
//! work for files as for pictures.
//!
//! The conversation is the front desk and a worker does each errand, so
//! every test scripts both: the front desk handing off and passing on, and
//! the worker using the tools.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use jiff::Zoned;
use tsetsen_core::ports::{
    ExportedPhoto, FileInfo, PhotoInfo, PhotoLibrary, PhotoQuery, Playback, SpeechToText,
    TextToSpeech, ThinkEvent, Thinker, Turn,
};
use tsetsen_core::{Ctx, Layer, RequestId, ServiceError};
use tsetsen_engine::{
    ActionOutcome, Capabilities, Clock, Conversation, ConversationEngine, ConversationEvent,
    Observer, Reply, ToolSet, Toolbox, TurnOutcome, catalog,
};
use tsetsen_policy::EgressPolicy;
use tsetsen_router::{Route, Router};
use tsetsen_testkit::{
    FakeContacts, FakeFiles, FakeLauncher, FakeMailer, FakeMemory, FakePhotos, FakePlayback,
    FakeShowcase, FakeStt, FakeThinker, FakeTts, contact, local, one_second_of_audio, tool_call,
};

fn router<P: ?Sized + tsetsen_core::Describe>(layer: Layer, provider: Arc<P>) -> Router<P> {
    let route = Route::new(provider.descriptor().id.clone());
    Router::new(layer, &route, vec![provider], Arc::new(EgressPolicy::new())).unwrap()
}

/// Sunday, November 1, 2026, mid-afternoon in Los Angeles.
fn clock() -> Clock {
    Arc::new(|| {
        "2026-11-01T15:00:00-08:00[America/Los_Angeles]"
            .parse::<Zoned>()
            .unwrap()
    })
}

/// A photo taken at a local time in Los Angeles.
fn photo(id: &str, when: &str) -> PhotoInfo {
    let taken: Zoned = format!("{when}[America/Los_Angeles]").parse().unwrap();
    PhotoInfo {
        id: id.into(),
        taken: SystemTime::from(taken.timestamp()),
        location: None,
        favorite: false,
    }
}

fn vacation() -> Vec<PhotoInfo> {
    vec![
        photo("beach-1", "2026-10-31T10:05:00-07:00"),
        photo("beach-2", "2026-10-31T10:40:00-07:00"),
        photo("beach-3", "2026-10-31T16:20:00-07:00"),
        photo("garden", "2026-08-12T12:00:00-07:00"),
    ]
}

struct Services {
    photos: Arc<dyn PhotoLibrary>,
    contacts: Arc<FakeContacts>,
    memory: Arc<FakeMemory>,
    showcase: Arc<FakeShowcase>,
    files: Arc<FakeFiles>,
    launcher: Arc<FakeLauncher>,
    mailer: Arc<FakeMailer>,
}

/// A file changed at a local time in Los Angeles.
fn file(path: &str, when: &str, bytes: u64) -> FileInfo {
    let modified: Zoned = format!("{when}[America/Los_Angeles]").parse().unwrap();
    FileInfo {
        path: path.into(),
        modified: SystemTime::from(modified.timestamp()),
        bytes,
    }
}

fn documents() -> Vec<FileInfo> {
    vec![
        file(
            "/Users/grandma/Documents/Bank letter.pdf",
            "2026-10-28T09:00:00-07:00",
            80_000,
        ),
        file(
            "/Users/grandma/Documents/Bank statement March.pdf",
            "2026-03-30T09:00:00-07:00",
            90_000,
        ),
        file(
            "/Users/grandma/Documents/Recipes.docx",
            "2026-06-01T09:00:00-07:00",
            20_000,
        ),
    ]
}

impl Services {
    fn new(photos: Arc<dyn PhotoLibrary>) -> Self {
        Self {
            photos,
            contacts: Arc::new(FakeContacts::new(vec![
                contact("g1", "David Smith", "david@example.com"),
                contact("g2", "Mary Jones", "mary@example.com"),
            ])),
            memory: Arc::new(FakeMemory::default()),
            showcase: Arc::new(FakeShowcase::default()),
            files: Arc::new(FakeFiles::new(documents())),
            launcher: Arc::new(FakeLauncher::default()),
            mailer: Arc::new(FakeMailer::default()),
        }
    }

    /// Everything a well-equipped computer offers.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            contacts: Some(Arc::clone(&self.contacts) as _),
            memory: Some(Arc::clone(&self.memory) as _),
            photos: Some(Arc::clone(&self.photos)),
            showcase: Some(Arc::clone(&self.showcase) as _),
            files: Some(Arc::clone(&self.files) as _),
            launcher: Some(Arc::clone(&self.launcher) as _),
            mail: Some(Arc::clone(&self.mailer) as _),
            desktop: None,
        }
    }

    fn tools(&self) -> ToolSet {
        catalog(self.capabilities(), clock()).unwrap().unwrap()
    }
}

/// Records what the user heard, cues included.
#[derive(Default)]
struct Ears(Mutex<Vec<String>>);

impl Observer for Ears {
    fn on_event(&self, event: ConversationEvent<'_>) {
        if let ConversationEvent::Speaking(text) = event {
            self.0.lock().unwrap().push(text.to_owned());
        }
    }
}

impl Ears {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

struct Setup {
    engine: ConversationEngine,
    /// The front desk's thinker.
    front: Arc<FakeThinker>,
    /// The workers' thinker.
    worker: Arc<FakeThinker>,
}

fn setup(
    heard: &[&str],
    front: Vec<Vec<ThinkEvent>>,
    worker: Vec<Vec<ThinkEvent>>,
    services: &Services,
) -> Setup {
    let stt: Arc<dyn SpeechToText> = Arc::new(FakeStt::hearing_in_turn(
        local("parakeet", Layer::Stt),
        heard,
    ));
    let front = Arc::new(FakeThinker::scripted(local("front", Layer::Think), front));
    let worker = Arc::new(FakeThinker::scripted(local("worker", Layer::Think), worker));
    let tts = Arc::new(FakeTts::new(
        local("supertonic", Layer::Speak),
        Duration::from_millis(30),
    ));
    let engine = ConversationEngine::new(
        router(Layer::Stt, stt),
        router(Layer::Think, Arc::clone(&front) as Arc<dyn Thinker>),
        router(Layer::Speak, tts as Arc<dyn TextToSpeech>),
        Arc::new(FakePlayback::new()) as Arc<dyn Playback>,
    )
    .with_clock(clock())
    .with_worker_thinker(router(
        Layer::Think,
        Arc::clone(&worker) as Arc<dyn Thinker>,
    ))
    .with_tools(Arc::new(services.tools()));
    Setup {
        engine,
        front,
        worker,
    }
}

fn text(t: &str) -> ThinkEvent {
    ThinkEvent::Text(t.into())
}

fn cx() -> Ctx {
    Ctx::new(RequestId::new(1))
}

async fn turn(s: &Setup, conversation: &mut Conversation, ears: &Ears) -> Reply {
    match s
        .engine
        .respond(conversation, &one_second_of_audio(), ears, &cx())
        .await
        .unwrap()
    {
        TurnOutcome::Replied(reply) => reply,
        other @ (TurnOutcome::NothingHeard { .. } | TurnOutcome::Announced(_)) => {
            panic!("expected a reply, got {other:?}")
        }
    }
}

/// The front desk hands the request to a new task.
fn start(what: &str) -> Vec<ThinkEvent> {
    vec![tool_call(
        "d1",
        "start_task",
        &serde_json::json!({ "what": what }).to_string(),
    )]
}

/// The front desk passes the user's words to task t1.
fn tell(message: &str) -> Vec<ThinkEvent> {
    vec![tool_call(
        "d2",
        "tell_task",
        &serde_json::json!({ "task": "t1", "message": message }).to_string(),
    )]
}

/// The tool results in `turns`.
fn tool_results(turns: &[Turn]) -> Vec<String> {
    turns
        .iter()
        .filter_map(|t| match t {
            Turn::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

/// Every tool result the worker was given in its latest request.
fn last_results(s: &Setup) -> Vec<String> {
    let request = s.worker.requests().pop().unwrap();
    request
        .turns
        .iter()
        .filter_map(|t| match t {
            Turn::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
#[allow(clippy::too_many_lines)] // one conversation, read top to bottom
async fn emails_her_son_the_vacation_pictures_after_one_yes() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &[
            "Hey kiddo, I need to email my son a picture of our vacation from last Saturday",
            "David",
            "Yes, that's him",
            "All of them",
            "Yes please",
        ],
        vec![
            // 1: hand it off at once.
            start("Email the user's son a picture of their vacation from last Saturday."),
            vec![text(
                "I found three pictures from yesterday, they're on your screen. What's your son's name?",
            )],
            // 2-4: the user's answers go to the same task.
            tell("His name is David."),
            vec![text("Is that David Smith?")],
            tell("Yes, that's him."),
            vec![text("Lovely. Which pictures would you like to send?")],
            tell("All of them."),
            // 5: after the yes.
            vec![text("It's sent. David should have it in a minute.")],
        ],
        vec![
            // 1: look before asking, both at once.
            vec![
                tool_call("t1", "find_person", r#"{"who":"my son"}"#),
                tool_call("t2", "find_pictures", r#"{"when":"last Saturday"}"#),
            ],
            vec![text(
                "I found 3 pictures from yesterday. What is your son's name?",
            )],
            // 2
            vec![tool_call("t3", "find_person", r#"{"who":"David"}"#)],
            vec![text("Is it David Smith?")],
            // 3
            vec![tool_call(
                "t4",
                "remember_person",
                r#"{"relation":"son","person":"c1"}"#,
            )],
            vec![text("Which pictures should I send?")],
            // 4: held for the user's yes, and read back by the engine.
            vec![tool_call(
                "t5",
                "share",
                r#"{"to":["c1"],"subject":"Our trip","message":"Here are the pictures from our trip. Love, Mom","items":["p1","p2","p3"]}"#,
            )],
            // 5
            vec![text("Sent.")],
        ],
        &services,
    );
    let ears = Ears::default();
    let mut conversation = Conversation::default();

    let first = turn(&s, &mut conversation, &ears).await;
    let results = last_results(&s);
    assert!(
        results[0].contains("Nobody is saved as the user's son"),
        "{results:?}"
    );
    assert!(
        results[1].contains("Found 3 pictures from Saturday, October 31")
            && results[1].contains("picture 1 is p1, taken at 10:05 AM"),
        "only yesterday has pictures, so no need to ask which Saturday: {results:?}"
    );
    assert_eq!(
        services.showcase.showing().unwrap().len(),
        3,
        "on the screen"
    );
    let heard = ears.take();
    assert!(
        heard[0].ends_with("moment.") || heard[0].starts_with("Sure"),
        "a task is acknowledged before anything else: {heard:?}"
    );
    assert!(first.spoken.ends_with("What's your son's name?"));

    turn(&s, &mut conversation, &ears).await;
    assert!(
        last_results(&s)
            .last()
            .unwrap()
            .contains("c1: David Smith, david@example.com")
    );

    turn(&s, &mut conversation, &ears).await;
    assert_eq!(services.memory.people_now()[0].relation, "son");

    let fourth = turn(&s, &mut conversation, &ears).await;
    assert!(
        services.mailer.sent().is_empty(),
        "nothing sent before the yes"
    );
    assert_eq!(fourth.actions[0].outcome, ActionOutcome::AwaitingApproval);
    assert_eq!(
        fourth.spoken,
        "I'll email your son, David Smith, at david at example dot com, 3 pictures, with the \
         subject \"Our trip\" and the message: \"Here are the pictures from our trip. Love, Mom\". \
         Shall I go ahead?",
        "read back word for word"
    );

    let fifth = turn(&s, &mut conversation, &ears).await;
    assert_eq!(fifth.actions[0].outcome, ActionOutcome::Done);
    let sent = services.mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to[0].address, "david@example.com");
    assert_eq!(sent[0].subject, "Our trip");
    assert_eq!(sent[0].attachments.len(), 3);
    assert_eq!(services.showcase.showing(), None, "taken off the screen");
    assert!(fifth.spoken.starts_with("It's sent."));

    // Next time, "my son" is known.
    let s = setup(
        &["Send my son a note"],
        vec![start("Send the user's son a note."), vec![text("Okay.")]],
        vec![
            vec![tool_call("t1", "find_person", r#"{"who":"my son"}"#)],
            vec![text("What should the note say?")],
        ],
        &services,
    );
    turn(&s, &mut Conversation::default(), &ears).await;
    assert!(
        last_results(&s)[0].starts_with("c1: David Smith, the user's son"),
        "{:?}",
        last_results(&s)
    );
}

#[tokio::test(start_paused = true)]
async fn asks_which_saturday_only_when_both_have_pictures() {
    let mut photos = vacation();
    photos.push(photo("park", "2026-10-24T11:00:00-07:00"));
    let library = Arc::new(FakePhotos::new(photos));
    let services = Services::new(Arc::clone(&library) as _);
    let s = setup(
        &["Show me the pictures from last Saturday"],
        vec![
            start("Show the pictures from last Saturday."),
            vec![text("Yesterday, or the Saturday before?")],
        ],
        vec![
            vec![tool_call(
                "t1",
                "find_pictures",
                r#"{"when":"last Saturday"}"#,
            )],
            vec![text("Yesterday, or the Saturday before?")],
        ],
        &services,
    );
    turn(&s, &mut Conversation::default(), &Ears::default()).await;
    let result = &last_results(&s)[0];
    assert!(
        result.contains("Saturday, October 31 has 3 pictures")
            && result.contains("Saturday, October 24 has 1 picture"),
        "{result}"
    );
    assert!(
        library.exports().is_empty(),
        "nothing shown until the user says"
    );
}

#[tokio::test(start_paused = true)]
async fn offers_the_nearest_days_when_nothing_matches() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &["the pictures from Christmas"],
        vec![
            start("Show the pictures from Christmas."),
            vec![text("Hmm.")],
        ],
        vec![
            vec![tool_call("t1", "find_pictures", r#"{"when":"Christmas"}"#)],
            vec![text("There are none from Christmas.")],
        ],
        &services,
    );
    turn(&s, &mut Conversation::default(), &Ears::default()).await;
    let result = &last_results(&s)[0];
    assert!(
        result.starts_with("There are no pictures from Christmas 2025.")
            && result.contains("Saturday, October 31: 3 pictures; Wednesday, August 12: 1 picture"),
        "{result}"
    );
}

#[tokio::test(start_paused = true)]
async fn only_found_people_and_pictures_can_be_emailed() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    for args in [
        r#"{"to":["attacker@example.com"],"subject":"Hi","message":"Hello"}"#,
        r#"{"to":["c9"],"subject":"Hi","message":"Hello"}"#,
    ] {
        let s = setup(
            &["email my son"],
            vec![start("Email the user's son."), vec![text("Sorry.")]],
            vec![
                vec![tool_call("t1", "share", args)],
                vec![text("Sorry, I could not.")],
            ],
            &services,
        );
        let reply = turn(&s, &mut Conversation::default(), &Ears::default()).await;
        assert!(
            matches!(&reply.actions[0].outcome, ActionOutcome::Failed(why) if why.contains("from find_person")),
            "{:?}",
            reply.actions
        );
        assert_eq!(reply.spoken, "Sorry.");
    }
    assert!(services.mailer.sent().is_empty());
}

/// A library that takes its time, like a large one on a slow disk.
struct SlowPhotos(FakePhotos);

#[async_trait]
impl PhotoLibrary for SlowPhotos {
    async fn search(&self, query: &PhotoQuery, cx: &Ctx) -> Result<Vec<PhotoInfo>, ServiceError> {
        tokio::time::sleep(Duration::from_secs(12)).await;
        self.0.search(query, cx).await
    }

    async fn export(
        &self,
        ids: &[String],
        max_pixels: u32,
        cx: &Ctx,
    ) -> Result<Vec<ExportedPhoto>, ServiceError> {
        self.0.export(ids, max_pixels, cx).await
    }
}

#[tokio::test(start_paused = true)]
async fn slow_work_is_acknowledged_narrated_then_finished_in_the_background() {
    let services = Services::new(Arc::new(SlowPhotos(FakePhotos::new(vacation()))));
    let s = setup(
        &["Find yesterday's pictures"],
        vec![
            start("Find yesterday's pictures."),
            vec![text("I'm still looking. I'll tell you when they're ready.")],
            vec![text("They're on your screen.")],
        ],
        vec![
            vec![tool_call("t1", "find_pictures", r#"{"when":"yesterday"}"#)],
            vec![text("Three pictures from yesterday are on the screen.")],
        ],
        &services,
    );
    let ears = Ears::default();
    let mut conversation = Conversation::default();
    let reply = turn(&s, &mut conversation, &ears).await;
    let heard = ears.take();
    assert!(heard[0].contains("moment"), "acknowledged: {heard:?}");
    assert_eq!(heard[1], "Looking through your pictures.", "narrated once");
    assert_eq!(
        heard[2..],
        ["I'm still looking.", "I'll tell you when they're ready."]
    );
    assert_eq!(
        reply.spoken, "I'm still looking. I'll tell you when they're ready.",
        "past the desk's patience, the user is free to carry on; fillers are not part of the conversation"
    );
    assert!(services.showcase.showing().is_none(), "not yet");

    // The search finishes later, and the news is told when the user is quiet.
    let mut news = s.engine.news().unwrap();
    while !s.engine.has_news() {
        news.changed().await.unwrap();
    }
    let announced = s
        .engine
        .announce(&mut conversation, &ears, &cx())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(announced.spoken, "They're on your screen.");
    assert_eq!(services.showcase.showing().unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn narration_between_hand_offs_is_not_spoken() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &["email my son yesterday's pictures"],
        vec![
            start("Find out who the user's son is."),
            vec![
                text("In the meantime, let me find those pictures."),
                tool_call(
                    "d2",
                    "start_task",
                    r#"{"what":"Find yesterday's pictures."}"#,
                ),
            ],
            vec![text("I found three pictures. What's your son's name?")],
        ],
        vec![
            vec![tool_call("t1", "find_person", r#"{"who":"my son"}"#)],
            vec![text("What is your son's name?")],
            vec![tool_call("t2", "find_pictures", r#"{"when":"yesterday"}"#)],
            vec![text("Three pictures from yesterday are on the screen.")],
        ],
        &services,
    );
    let ears = Ears::default();
    let reply = turn(&s, &mut Conversation::default(), &ears).await;
    assert_eq!(
        reply.spoken,
        "I found three pictures. What's your son's name?"
    );
    assert!(
        !ears.take().iter().any(|h| h.contains("meantime")),
        "never heard"
    );
}

#[tokio::test(start_paused = true)]
async fn saying_it_will_act_without_acting_hands_her_words_on() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &["Send David a note saying I love him"],
        vec![
            // Chooses to just talk, then says it will, handing nothing off.
            vec![tool_call("d0", "just_talk", "{}")],
            vec![text("Great! I'll send David Smith your note now.")],
        ],
        vec![
            vec![tool_call("t1", "find_person", r#"{"who":"David"}"#)],
            vec![tool_call(
                "t2",
                "share",
                r#"{"to":["c1"],"subject":"A note","message":"I love you."}"#,
            )],
        ],
        &services,
    );
    let reply = turn(&s, &mut Conversation::default(), &Ears::default()).await;
    assert_eq!(
        reply.actions.last().unwrap().outcome,
        ActionOutcome::AwaitingApproval,
        "the promise led to a real, held send"
    );
    assert_eq!(
        reply.spoken,
        "Great! I'll email David Smith, at david at example dot com, with the subject \"A note\" \
         and the message: \"I love you.\" Shall I go ahead?",
        "the empty promise is never said; the read-back is"
    );
    assert_eq!(s.front.requests().len(), 2, "no step to correct it");
    let brief = &s.worker.requests()[0].turns[0];
    assert!(
        matches!(brief, Turn::User { text } if text.starts_with("Send David a note saying I love him")),
        "her own words: {brief:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn claiming_it_sent_when_nothing_ran_is_corrected() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &["Yes please"],
        vec![
            vec![text("I've sent the pictures to David!")],
            vec![text(
                "Sorry, I haven't sent anything yet. Who should I send them to?",
            )],
        ],
        vec![],
        &services,
    );
    let mut conversation = Conversation::default();
    let reply = turn(&s, &mut conversation, &Ears::default()).await;
    assert!(
        reply.spoken.ends_with("Who should I send them to?"),
        "{}",
        reply.spoken
    );
    assert!(
        conversation
            .turns()
            .iter()
            .all(|t| !matches!(t, Turn::User { text } if text.contains("system"))),
        "the correction is not kept in the conversation"
    );
}

#[tokio::test(start_paused = true)]
async fn learns_who_her_son_is_from_an_approved_email_and_says_what_is_missing() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &["Email my son yesterday's pictures", "David", "Yes"],
        vec![
            start("Email the user's son yesterday's pictures."),
            vec![text("What's your son's name?")],
            tell("His name is David."),
            vec![text("Done.")],
        ],
        vec![
            vec![
                tool_call("t1", "find_person", r#"{"who":"my son"}"#),
                tool_call("t2", "find_pictures", r#"{"when":"yesterday"}"#),
            ],
            vec![text("What is your son's name?")],
            vec![tool_call("t3", "find_person", r#"{"who":"David"}"#)],
            // Forgets the pictures, and never calls remember_person.
            vec![tool_call(
                "t4",
                "share",
                r#"{"to":["c1"],"subject":"Hi","message":"Hi David, here you go."}"#,
            )],
            vec![text("Sent.")],
        ],
        &services,
    );
    let mut conversation = Conversation::default();
    let ears = Ears::default();
    turn(&s, &mut conversation, &ears).await;
    let asked = turn(&s, &mut conversation, &ears).await;
    assert!(
        asked.spoken.starts_with(
            "I'll email your son, David Smith, at david at example dot com, with no pictures,"
        ),
        "the relation is read back, and so are the missing pictures: {}",
        asked.spoken
    );
    turn(&s, &mut conversation, &ears).await;
    assert!(last_results(&s).last().unwrap().contains(
        "Sent to David Smith, with no pictures. Remembered that David Smith is the user's son."
    ));
    assert_eq!(services.memory.people_now()[0].contact.name, "David Smith");
}

#[tokio::test(start_paused = true)]
async fn sends_a_file_the_same_way_as_pictures() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &[
            "Can you send David the letter from the bank I got this week",
            "Yes",
        ],
        vec![
            start("Send David the letter from the bank the user got this week."),
            vec![text("Sent.")],
        ],
        vec![
            vec![
                tool_call("t1", "find_person", r#"{"who":"David"}"#),
                tool_call(
                    "t2",
                    "find_files",
                    r#"{"about":"the letter from the bank","when":"this week"}"#,
                ),
            ],
            vec![tool_call(
                "t3",
                "share",
                r#"{"to":["c1"],"subject":"The bank letter","message":"Hi David, can you look at this for me?","items":["f1"]}"#,
            )],
            vec![text("Sent.")],
        ],
        &services,
    );
    let mut conversation = Conversation::default();
    let ears = Ears::default();
    let asked = turn(&s, &mut conversation, &ears).await;
    let results = &tool_results(&s.worker.requests()[1].turns);
    assert!(
        results[1].starts_with("Found 1 file about letter bank from this week")
            && results[1].contains("f1: \"Bank letter\""),
        "the statement from March is not this week: {results:?}"
    );
    assert_eq!(services.files.queries()[0].words, ["letter", "bank"]);
    assert_eq!(
        asked.spoken,
        "I'll email David Smith, at david at example dot com, the file \"Bank letter\", with \
         the subject \"The bank letter\" and the message: \"Hi David, can you look at this for \
         me?\" Shall I go ahead?"
    );
    turn(&s, &mut conversation, &ears).await;
    let sent = services.mailer.sent();
    assert_eq!(
        sent[0].attachments,
        [std::path::PathBuf::from(
            "/Users/grandma/Documents/Bank letter.pdf"
        )]
    );
    assert!(
        last_results(&s)
            .last()
            .unwrap()
            .contains("Sent to David Smith, with the file \"Bank letter\".")
    );
}

#[tokio::test(start_paused = true)]
async fn shows_and_opens_what_it_found() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &["Where's my recipes file", "Open it please"],
        vec![
            start("Find the user's recipes file and show it."),
            vec![text("Here's one, but it can't be shown.")],
            tell("Open it please."),
            vec![text("It's open.")],
        ],
        vec![
            vec![tool_call(
                "t1",
                "find_files",
                r#"{"about":"my recipes file"}"#,
            )],
            vec![tool_call("t2", "show", r#"{"items":["f1"]}"#)],
            vec![text("I found it, but it cannot be shown. Shall I open it?")],
            vec![tool_call("t3", "open", r#"{"item":"f1"}"#)],
            vec![text("It is open.")],
        ],
        &services,
    );
    let mut conversation = Conversation::default();
    let ears = Ears::default();
    let first = turn(&s, &mut conversation, &ears).await;
    assert!(
        matches!(&first.actions[1].outcome, ActionOutcome::Failed(why) if why.contains("opened instead")),
        "a .docx cannot be previewed: {:?}",
        first.actions
    );
    let second = turn(&s, &mut conversation, &ears).await;
    assert_eq!(
        second.actions[0].outcome,
        ActionOutcome::Done,
        "opening needs no yes"
    );
    assert_eq!(
        services.launcher.opened(),
        [std::path::PathBuf::from(
            "/Users/grandma/Documents/Recipes.docx"
        )]
    );
}

#[tokio::test(start_paused = true)]
async fn found_pictures_can_be_opened_and_files_shown() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &["Show me the bank letter and open yesterday's first picture"],
        vec![
            start("Show the bank letter and open yesterday's first picture."),
            vec![text("There you are.")],
        ],
        vec![
            vec![
                tool_call("t1", "find_files", r#"{"about":"bank letter"}"#),
                tool_call("t2", "find_pictures", r#"{"when":"yesterday"}"#),
            ],
            vec![
                tool_call("t3", "show", r#"{"items":["f1"]}"#),
                tool_call("t4", "open", r#"{"item":"p1"}"#),
            ],
            vec![text("There you are.")],
        ],
        &services,
    );
    turn(&s, &mut Conversation::default(), &Ears::default()).await;
    assert_eq!(
        services.showcase.showing().unwrap(),
        [std::path::PathBuf::from(
            "/Users/grandma/Documents/Bank letter.pdf"
        )]
    );
    assert_eq!(
        services.launcher.opened().len(),
        1,
        "a picture opens like any file"
    );
}

#[test]
fn each_computer_gets_the_tools_its_services_support() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let names = |caps: Capabilities| -> Vec<String> {
        catalog(caps, clock())
            .unwrap()
            .map(|t| t.specs().into_iter().map(|s| s.name).collect())
            .unwrap_or_default()
    };
    let all = names(services.capabilities());
    for verb in [
        "find_person",
        "remember_person",
        "find_pictures",
        "find_files",
        "open",
        "show",
        "share",
    ] {
        assert!(all.iter().any(|n| n == verb), "{verb} missing from {all:?}");
    }
    // Files only, as on a computer with no address book, photos or email.
    let files_only = names(Capabilities {
        files: Some(Arc::clone(&services.files) as _),
        launcher: Some(Arc::clone(&services.launcher) as _),
        ..Capabilities::default()
    });
    assert_eq!(files_only, ["find_files", "open"]);
    // Email without a way to find people offers nothing to send with.
    let mail_only = names(Capabilities {
        mail: Some(Arc::clone(&services.mailer) as _),
        ..Capabilities::default()
    });
    assert!(mail_only.is_empty(), "{mail_only:?}");
}

#[tokio::test(start_paused = true)]
async fn a_new_task_can_use_what_an_earlier_one_found() {
    let services = Services::new(Arc::new(FakePhotos::new(vacation())));
    let s = setup(
        &[
            "Show me yesterday's pictures",
            "Send the first one to Mary",
            "Yes",
        ],
        vec![
            start("Show yesterday's pictures."),
            vec![text("Here they are.")],
            start("Send the first of yesterday's pictures to Mary."),
            vec![text("Sent.")],
        ],
        vec![
            vec![tool_call("t1", "find_pictures", r#"{"when":"yesterday"}"#)],
            vec![text("Three pictures from yesterday are on the screen.")],
            vec![tool_call("t2", "find_person", r#"{"who":"Mary"}"#)],
            vec![tool_call(
                "t3",
                "share",
                r#"{"to":["c1"],"message":"Hi Mary, a picture from yesterday.","items":["p1"]}"#,
            )],
            vec![text("Sent.")],
        ],
        &services,
    );
    let mut conversation = Conversation::default();
    let ears = Ears::default();
    turn(&s, &mut conversation, &ears).await;
    let asked = turn(&s, &mut conversation, &ears).await;
    let brief = &s.worker.requests()[2].turns[0];
    assert!(
        matches!(brief, Turn::User { text } if text.contains("Found earlier, usable by id: p1: a picture taken 2026-10-31")),
        "the second task is told what the first found: {brief:?}"
    );
    assert!(
        asked.spoken.contains("Mary Jones") && asked.spoken.contains("the picture"),
        "{}",
        asked.spoken
    );
    turn(&s, &mut conversation, &ears).await;
    assert_eq!(services.mailer.sent()[0].attachments.len(), 1);
}
