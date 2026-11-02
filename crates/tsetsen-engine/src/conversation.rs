//! Conversation: speech in, a spoken reply out, thinker-talker style, with
//! tools.
//!
//! The thinker streams its reply; the talker renders it a clause at a time;
//! playback of one clause overlaps rendering of the next. The user hears the
//! first words about one clause after the thinker starts writing.
//!
//! With tools, the conversation is the front desk: it never does the work
//! itself. It hands each request to a worker as a task (see [`crate::desk`])
//! and keeps talking; workers run side by side and report back. Anything
//! that may commit (send, delete, submit) is held until the user says yes.
//! That answer is read from their own words by [`tsetsen_policy::approval`],
//! never by a model, so nothing on screen can approve an action.
//!
//! The user is never left in silence wondering whether they were heard: a
//! short acknowledgement plays as soon as a task begins, slow steps are
//! narrated ("Looking through your pictures"), and long waits get a "still
//! working on it". These come from [`Pacing`], timed against what is really
//! happening, not from the model.
//!
//! If the user interrupts, the reply stops at once and the history records
//! only what they actually heard, plus every action that ran, so the next turn
//! builds on what really happened.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
// Tokio's clock: identical in production, controllable in tests.
use tokio::time::Instant;
use tsetsen_core::ports::{
    AudioClip, Playback, SpeechRequest, SpeechToText, SttHints, TextToSpeech, ThinkEvent,
    ThinkRequest, ThinkStream, Thinker, ToolCall, ToolSpec, Turn,
};
use tsetsen_core::{
    Ctx, Interrupted, LanguageTag, ProviderError, ProviderId, Sensitivity, VoiceId,
};
use tsetsen_policy::{Approval, approval};
use tsetsen_router::{Exclusion, Router};

use crate::cancel::cancellable;
use crate::claims::{self, Claim};
use crate::desk::{
    Asked, DESK_INSTRUCTIONS, Desk, DeskConfig, Dispatched, JUST_TALK, Update, confirmation,
};
use crate::errands::Clock;
use crate::speakable::ClauseChunker;
use crate::stt::{TRANSCRIPT_LABEL, TranscribeError, transcribe};
use crate::tools::Toolbox;

/// Who the assistant is and how it talks. Written for people who are not
/// comfortable with technology, which makes it clearer for everyone.
pub const VOICE_INSTRUCTIONS: &str = "\
You are Tsetsen, a warm, patient helper who lives on the user's computer and talks with them out \
loud. Many of the people you help are older and not comfortable with technology. Everything you \
write is spoken aloud.
- Be kind, calm and unhurried, like a helpful grandchild. Never rush, scold or make the user feel \
slow. If they call you a pet name like kiddo or dear, that is affection: go along with it.
- Keep it short: one to three short sentences. Say the result first.
- Use everyday words. Never mention apps, windows, files, settings, tools, ids or errors unless \
the user does: say your pictures, your email.
- Ask at most one question at a time, and make it easy to answer: two choices, or yes or no.
- Plain spoken language only: no markdown, lists, emoji or links. Say numbers, dates and times \
the way people say them.
- The user's words come from speech recognition and may contain mistakes. Make sensible guesses; \
if one word matters and you are unsure of it, ask about just that word.
- If you were interrupted, your previous reply ends where the user stopped hearing it.";

/// When to fill silence while a reply is under way, and with what.
///
/// Times count from the last moment the user heard anything: the end of
/// their own turn, or the last thing played.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pacing {
    /// Said when a task starts, or when nothing has been said for
    /// `acknowledge_after`: "Okay, one moment."
    pub acknowledgements: Vec<String>,
    /// Said during long waits: "Still working on it."
    pub still_working: Vec<String>,
    /// Silence before an acknowledgement, if no task has started.
    pub acknowledge_after: Duration,
    /// Silence before a slow step is narrated.
    pub narrate_after: Duration,
    /// Silence before "still working", and between repeats of it.
    pub still_after: Duration,
    /// The most "still working"s in one reply.
    pub max_still: usize,
}

impl Default for Pacing {
    fn default() -> Self {
        let phrases = |p: &[&str]| p.iter().map(|&s| s.to_owned()).collect();
        Self {
            acknowledgements: phrases(&[
                "Okay, one moment.",
                "Sure, let me see.",
                "Alright, give me a second.",
                "Of course, one moment.",
            ]),
            still_working: phrases(&["Still working on it.", "Nearly there.", "Bear with me."]),
            acknowledge_after: Duration::from_millis(1500),
            narrate_after: Duration::from_millis(1200),
            still_after: Duration::from_secs(7),
            max_still: 2,
        }
    }
}

impl Pacing {
    /// Every fixed phrase, for rendering ahead of time.
    fn phrases(&self) -> impl Iterator<Item = &String> {
        self.acknowledgements.iter().chain(&self.still_working)
    }
}

/// Requests that may carry screen content, from tool results.
const SCREEN_LABEL: Sensitivity = Sensitivity::ScreenContent;

/// Settings for conversation.
#[derive(Clone, Debug)]
pub struct ConversationConfig {
    /// Persona and reply style, sent as the system prompt.
    pub instructions: String,
    /// Voice for the talker; the provider's default if `None`.
    pub voice: Option<VoiceId>,
    /// Conversation language, if fixed. Routes to providers that support it.
    pub language: Option<LanguageTag>,
    /// Turns of history sent to the thinker; older turns are dropped.
    pub max_turns: usize,
    /// How tasks run, and when to ask before acting.
    pub desk: DeskConfig,
    /// The most thinker calls in one reply. Caps a model that loops.
    pub max_steps: usize,
    /// Filling silence while working. `None` stays silent until the thinker
    /// speaks.
    pub pacing: Option<Pacing>,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            instructions: VOICE_INSTRUCTIONS.into(),
            voice: None,
            language: None,
            max_turns: 40,
            desk: DeskConfig::default(),
            max_steps: 8,
            pacing: Some(Pacing::default()),
        }
    }
}

/// The conversation so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conversation {
    turns: Vec<Turn>,
}

impl Conversation {
    /// The turns, oldest first.
    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Adds what the user said. Speaking again before the assistant replied
    /// continues the same turn.
    fn push_user(&mut self, text: &str) {
        match self.turns.last_mut() {
            Some(Turn::User { text: last }) => {
                last.push(' ');
                last.push_str(text);
            }
            _ => self.turns.push(Turn::user(text)),
        }
    }

    /// About the last `max` turns, starting with something the user said or
    /// something that happened, so a tool result is never separated from its
    /// call. Always includes the latest of those, even if that means more
    /// than `max`.
    fn recent(&self, max: usize) -> Vec<Turn> {
        let Some(last) = self.turns.iter().rposition(Turn::opens_exchange) else {
            return Vec::new();
        };
        let mut start = self.turns.len().saturating_sub(max.max(1)).min(last);
        while !self.turns[start].opens_exchange() {
            start += 1;
        }
        self.turns[start..].to_vec()
    }
}

/// How an action ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    /// It ran and reported success.
    Done,
    /// It was invalid or failed; the reason.
    Failed(String),
    /// Held for the user's OK.
    AwaitingApproval,
    /// The user did not approve it.
    Declined,
    /// The user interrupted while it ran. It may or may not have happened.
    Interrupted,
}

/// One action the assistant took or proposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRecord {
    /// What it was: "open Google Chrome".
    pub summary: String,
    /// How it ended.
    pub outcome: ActionOutcome,
}

/// Progress, for showing the conversation as it happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationEvent<'a> {
    /// The user started talking.
    Listening,
    /// What the user said.
    Heard(&'a str),
    /// A clause is starting to play.
    Speaking(&'a str),
    /// An action finished, failed, or is waiting for approval.
    Action(&'a ActionRecord),
    /// The user talked over the reply; it stopped.
    Interrupted,
    /// Something failed; the conversation continues.
    Problem(&'a str),
}

/// Receives conversation progress. Must return quickly.
pub trait Observer: Send + Sync {
    /// Called for each event.
    fn on_event(&self, event: ConversationEvent<'_>);
}

/// An observer that ignores everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unobserved;

impl Observer for Unobserved {
    fn on_event(&self, _: ConversationEvent<'_>) {}
}

/// Where the time went, measured from the end of the user's turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyTiming {
    /// Speech-to-text finished.
    pub transcribed: Duration,
    /// The thinker's first words arrived, if any did.
    pub first_text: Option<Duration>,
    /// The first audio started playing, if any did.
    pub first_audio: Option<Duration>,
    /// The reply finished or stopped.
    pub finished: Duration,
}

/// One exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// What the user said.
    pub heard: String,
    /// Everything the thinker wrote, across all steps.
    pub written: String,
    /// What the user actually heard. Shorter than `written` if interrupted.
    pub spoken: String,
    /// Actions taken or proposed, in order.
    pub actions: Vec<ActionRecord>,
    /// Whether the user cut the reply short.
    pub interrupted: bool,
    /// Why the reply was cut short by a failure, if it was.
    pub degraded: Option<String>,
    /// Latency breakdown.
    pub timing: ReplyTiming,
    /// The speech-to-text provider used; `None` when the assistant spoke up
    /// on its own.
    pub stt: Option<ProviderId>,
    /// The thinker used.
    pub thinker: Option<ProviderId>,
    /// The talker used for the first clause.
    pub talker: Option<ProviderId>,
}

/// The result of one user turn.
// One per turn, matched immediately: boxing `Reply` would cost an allocation
// and ergonomics for no measurable gain.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The assistant replied, fully or until interrupted.
    Replied(Reply),
    /// The assistant spoke up on its own, with news from a task.
    Announced(Reply),
    /// The audio contained no words.
    NothingHeard {
        /// The provider that transcribed it.
        stt: ProviderId,
    },
}

/// A turn could not produce a reply at all.
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    /// Speech could not be transcribed.
    #[error(transparent)]
    Transcribe(#[from] TranscribeError),
    /// Policy or language support excluded every thinker.
    #[error("no thinker may handle this conversation")]
    NoThinker {
        /// Why each was excluded.
        excluded: Vec<Exclusion>,
    },
    /// Every eligible thinker failed to start a reply.
    #[error("every thinker failed")]
    ThinkFailed {
        /// Each provider tried, with its error.
        failures: Vec<(ProviderId, ProviderError)>,
    },
    /// Cancelled before the user's words were understood.
    #[error(transparent)]
    Interrupted(#[from] Interrupted),
}

/// A rendered clause on its way to the speaker.
struct Rendered {
    step: usize,
    text: String,
    audio: AudioClip,
}

/// One thinker call within a reply.
#[derive(Default)]
struct Step {
    written: String,
    calls: Vec<ToolCall>,
    results: HashMap<String, String>,
}

/// What the producing half of a reply learned.
struct Produced {
    steps: Vec<Step>,
    actions: Vec<ActionRecord>,
    /// Work was handed to a task in this reply.
    dispatched: bool,
    thinker: Option<ProviderId>,
    talker: Option<ProviderId>,
    first_text: Option<Duration>,
    problem: Option<String>,
}

/// What the user heard, clause by clause, tagged with its step.
struct Heard {
    clauses: Vec<(usize, String)>,
    first_audio: Option<Duration>,
    problem: Option<String>,
}

/// What the producer is doing, for the consumer to narrate.
#[derive(Clone, Debug, Default)]
enum Busy {
    /// Thinking or speaking.
    #[default]
    No,
    /// A tool is running, with what to say if it is slow.
    Running(Option<String>),
}

/// What the producer is doing, and how many tools it has run.
#[derive(Clone, Debug, Default)]
struct Activity {
    busy: Busy,
    /// Counts tool runs, so each is narrated at most once.
    serial: u64,
}

/// What the consumer has said to fill silence in one reply.
struct Filler {
    said_anything: bool,
    quiet_since: Instant,
    narrated: u64,
    still: usize,
}

/// Runs conversational turns.
pub struct ConversationEngine {
    stt: Router<dyn SpeechToText>,
    thinker: Arc<Router<dyn Thinker>>,
    talker: Router<dyn TextToSpeech>,
    playback: Arc<dyn Playback>,
    tools: Option<Arc<dyn Toolbox>>,
    /// The thinker for workers, if not the conversation's own.
    worker_thinker: Option<Arc<Router<dyn Thinker>>>,
    /// Built on first use, from the final configuration.
    desk: OnceLock<Arc<Desk>>,
    config: ConversationConfig,
    clock: Option<Clock>,
    /// Rendered fixed phrases, by text.
    cues: Mutex<HashMap<String, AudioClip>>,
    /// Rotates through phrases so they do not repeat back to back.
    cue_turn: AtomicUsize,
}

impl ConversationEngine {
    /// An engine with the default config and no tools.
    #[must_use]
    pub fn new(
        stt: Router<dyn SpeechToText>,
        thinker: Router<dyn Thinker>,
        talker: Router<dyn TextToSpeech>,
        playback: Arc<dyn Playback>,
    ) -> Self {
        Self {
            stt,
            thinker: Arc::new(thinker),
            talker,
            playback,
            tools: None,
            worker_thinker: None,
            desk: OnceLock::new(),
            config: ConversationConfig::default(),
            clock: None,
            cues: Mutex::new(HashMap::new()),
            cue_turn: AtomicUsize::new(0),
        }
    }

    /// Tells the thinker today's date, from `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Replaces the config.
    #[must_use]
    pub fn with_config(mut self, config: ConversationConfig) -> Self {
        self.config = config;
        self
    }

    /// Lets the assistant do things with `tools`, through tasks.
    #[must_use]
    pub fn with_tools(mut self, tools: Arc<dyn Toolbox>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Runs tasks with `thinker` instead of the conversation's own: a quick
    /// model to talk, a stronger one to work, or the other way round.
    #[must_use]
    pub fn with_worker_thinker(mut self, thinker: Router<dyn Thinker>) -> Self {
        self.worker_thinker = Some(Arc::new(thinker));
        self
    }

    /// The desk, if there are tools to work with.
    fn desk(&self) -> Option<&Arc<Desk>> {
        let tools = self.tools.as_ref()?;
        Some(self.desk.get_or_init(|| {
            Arc::new(Desk::new(
                Arc::clone(self.worker_thinker.as_ref().unwrap_or(&self.thinker)),
                Arc::clone(tools),
                self.config.desk.clone(),
                self.clock.clone(),
                self.config.language.clone(),
            ))
        }))
    }

    /// What the user is being asked to approve, if anything.
    #[must_use]
    pub fn awaiting_approval(&self) -> Option<String> {
        self.desk()?.asked().map(|asked| asked.summary)
    }

    /// Whether there is news from a task to tell the user, and nothing they
    /// must answer first. See [`ConversationEngine::announce`].
    #[must_use]
    pub fn has_news(&self) -> bool {
        self.desk().is_some_and(|desk| desk.has_news())
    }

    /// Changes whenever a task has news, for waking up to it.
    #[must_use]
    pub fn news(&self) -> Option<watch::Receiver<u64>> {
        self.desk().map(|desk| desk.news())
    }

    /// Runs the thinker once with the real instructions and tools, so a
    /// server that caches prompt prefixes has them ready for the first turn.
    pub async fn warm_up(&self, cx: &Ctx) -> Result<(), ConversationError> {
        let request = ThinkRequest {
            must_use_tool: false,
            instructions: self.instructions(),
            turns: vec![Turn::user("Say OK.")],
            language: self.config.language.clone(),
            tools: self.specs(),
        };
        self.drain(&self.thinker, &request, cx).await?;
        if let (Some(desk), Some(tools)) = (self.desk(), &self.tools) {
            let request = ThinkRequest {
                must_use_tool: false,
                instructions: desk.worker_instructions(),
                turns: vec![Turn::user("Say OK.")],
                language: self.config.language.clone(),
                tools: tools.specs(),
            };
            let thinker = self.worker_thinker.as_ref().unwrap_or(&self.thinker);
            self.drain(thinker, &request, cx).await?;
        }
        if let Some(pacing) = &self.config.pacing {
            for phrase in pacing.phrases() {
                self.cue(phrase, self.config.language.as_ref(), cx).await;
            }
        }
        Ok(())
    }

    /// Runs one request to its end, for warming up.
    async fn drain(
        &self,
        thinker: &Router<dyn Thinker>,
        request: &ThinkRequest,
        cx: &Ctx,
    ) -> Result<(), ConversationError> {
        let (_, mut stream) = start_thinking(thinker, request, self.label(), cx).await?;
        while let Some(event) = cancellable(cx, stream.next()).await? {
            if event.is_err() {
                break; // the warm-up worked if the stream started
            }
        }
        Ok(())
    }

    /// The front desk's tools, if there is work it can hand out.
    fn specs(&self) -> Vec<ToolSpec> {
        if self.tools.is_some() {
            Desk::specs()
        } else {
            Vec::new()
        }
    }

    /// Requests carrying task reports may carry screen content.
    fn label(&self) -> Sensitivity {
        if self.tools.is_some() {
            SCREEN_LABEL
        } else {
            TRANSCRIPT_LABEL
        }
    }

    fn instructions(&self) -> String {
        let mut instructions = self.config.instructions.clone();
        if self.tools.is_some() {
            instructions.push_str("\n\n");
            instructions.push_str(DESK_INSTRUCTIONS);
        }
        // The date only, so the prompt (and any cache of it) changes daily.
        if let Some(clock) = &self.clock {
            let today = clock();
            let _ = write!(
                instructions,
                "\n\nToday is {}.",
                today.strftime("%A, %B %-d, %Y")
            );
        }
        instructions
    }

    /// Hears `audio`, replies aloud, and records the exchange in
    /// `conversation`. Cancel `cx` to interrupt: the reply stops at once and
    /// `Ok` is still returned, with what was heard so far.
    pub async fn respond(
        &self,
        conversation: &mut Conversation,
        audio: &AudioClip,
        observer: &dyn Observer,
        cx: &Ctx,
    ) -> Result<TurnOutcome, ConversationError> {
        let started = Instant::now();
        let hints = SttHints {
            language: self.config.language.clone(),
            ..SttHints::default()
        };
        let (stt, transcript) = cancellable(cx, transcribe(&self.stt, audio, &hints, cx))
            .await
            .map_err(TranscribeError::from)??;
        let heard = transcript.text.trim().to_owned();
        if heard.is_empty() {
            return Ok(TurnOutcome::NothingHeard { stt });
        }
        observer.on_event(ConversationEvent::Heard(&heard));
        conversation.push_user(&heard);
        let transcribed = started.elapsed();
        let language = transcript.language.or_else(|| self.config.language.clone());

        let mut reply = Reply {
            heard,
            written: String::new(),
            spoken: String::new(),
            actions: Vec::new(),
            interrupted: false,
            degraded: None,
            timing: ReplyTiming {
                transcribed,
                first_text: None,
                first_audio: None,
                finished: transcribed,
            },
            stt: Some(stt),
            thinker: None,
            talker: None,
        };

        if let Some(desk) = self.desk()
            && let Some(asked) = desk.asked()
        {
            self.answer(desk, &asked, conversation, &mut reply, observer, cx)
                .await;
        }
        if cx.check().is_ok() {
            self.reply_aloud(
                conversation,
                language.as_ref(),
                &mut reply,
                observer,
                started,
                true,
                cx,
            )
            .await?;
        }
        Ok(TurnOutcome::Replied(finish(reply, observer, started, cx)))
    }

    /// Speaks up with news from tasks: what they found or asked, or what
    /// they need an OK for. Run it when the user is quiet and
    /// [`Self::has_news`]. `None` if there was nothing to say after all.
    pub async fn announce(
        &self,
        conversation: &mut Conversation,
        observer: &dyn Observer,
        cx: &Ctx,
    ) -> Result<Option<Reply>, ConversationError> {
        let Some(desk) = self.desk() else {
            return Ok(None);
        };
        if desk.asked().is_some() {
            return Ok(None); // the user's answer comes first
        }
        let started = Instant::now();
        let news = desk.take_news();
        let mut reply = Reply {
            heard: String::new(),
            written: String::new(),
            spoken: String::new(),
            actions: Vec::new(),
            interrupted: false,
            degraded: None,
            timing: ReplyTiming {
                transcribed: Duration::ZERO,
                first_text: None,
                first_audio: None,
                finished: Duration::ZERO,
            },
            stt: None,
            thinker: None,
            talker: None,
        };
        for delivery in &news {
            conversation.turns.push(Turn::Event {
                from: format!("task {}", delivery.task),
                text: delivery.describe(),
            });
            record(&delivery.actions, &mut reply.actions, observer);
        }
        // A request for an OK is read back as it is; anything else is put in
        // the front desk's own words.
        let think = news
            .iter()
            .any(|d| !matches!(d.update, Update::NeedsApproval(_)));
        if !think && !desk.has_news() {
            return Ok(None);
        }
        let language = self.config.language.clone();
        self.reply_aloud(
            conversation,
            language.as_ref(),
            &mut reply,
            observer,
            started,
            think,
            cx,
        )
        .await?;
        Ok(Some(finish(reply, observer, started, cx)))
    }

    /// Settles the open question with the user's latest words: a clear yes
    /// runs the held action; anything else withdraws it.
    async fn answer(
        &self,
        desk: &Arc<Desk>,
        asked: &Asked,
        conversation: &mut Conversation,
        reply: &mut Reply,
        observer: &dyn Observer,
        cx: &Ctx,
    ) {
        let from = format!("task {}", asked.task);
        if approval(&reply.heard) != Approval::Approved {
            desk.decline(asked, &reply.heard);
            let declined = ActionRecord {
                summary: asked.summary.clone(),
                outcome: ActionOutcome::Declined,
            };
            record(
                std::slice::from_ref(&declined),
                &mut reply.actions,
                observer,
            );
            conversation.turns.push(Turn::Event {
                from,
                text: format!(
                    "The user did not approve: {}. Nothing was done. If they want a change, pass \
                     it to {} with tell_task.",
                    asked.summary, asked.task
                ),
            });
            return;
        }
        // Say "okay" while it runs, so a slow send is not met with silence.
        let ack = async {
            if let Some(phrase) = self.next_phrase(|p| &p.acknowledgements) {
                let language = self.config.language.as_ref();
                if let Some(clip) = self.cue(&phrase, language, cx).await {
                    observer.on_event(ConversationEvent::Speaking(&phrase));
                    let _ = self.playback.play(&clip, cx).await;
                }
            }
        };
        let (delivery, ()) = tokio::join!(desk.approve(asked, cx), ack);
        let text = match &delivery {
            Some(delivery) => {
                record(&delivery.actions, &mut reply.actions, observer);
                format!("The user approved. {}", delivery.describe())
            }
            None => format!(
                "The user approved. {} is doing it now and will report when it is done.",
                asked.task
            ),
        };
        conversation.turns.push(Turn::Event { from, text });
    }

    /// Thinks, speaks and acts until the thinker is done, the step budget is
    /// spent, or the user interrupts.
    #[allow(clippy::too_many_arguments)]
    async fn reply_aloud(
        &self,
        conversation: &mut Conversation,
        language: Option<&LanguageTag>,
        reply: &mut Reply,
        observer: &dyn Observer,
        started: Instant,
        think: bool,
        cx: &Ctx,
    ) -> Result<(), ConversationError> {
        let history = conversation.recent(self.config.max_turns);
        // One clause buffered: enough to hide rendering time, small enough
        // that an interruption wastes little work.
        let (tx, rx) = mpsc::channel::<Rendered>(1);
        let (activity_tx, activity_rx) = watch::channel(Activity::default());
        let produce = self.produce(
            history,
            language,
            tx,
            &activity_tx,
            observer,
            started,
            think,
            cx,
        );
        let consume = self.consume(rx, activity_rx, language, observer, started, cx);
        let (produced, heard) = tokio::join!(produce, consume);
        let produced = produced?;

        // History: what was heard, every call made, every result.
        let mut spoken_all = Vec::new();
        for (index, step) in produced.steps.iter().enumerate() {
            let spoken: Vec<&str> = heard
                .clauses
                .iter()
                .filter(|(s, text)| *s == index && !text.is_empty())
                .map(|(_, text)| text.as_str())
                .collect();
            let spoken = spoken.join(" ");
            if spoken.is_empty() && step.calls.is_empty() {
                continue;
            }
            spoken_all.push(spoken.clone());
            conversation.turns.push(Turn::Assistant {
                text: spoken,
                tool_calls: step.calls.clone(),
            });
            for call in &step.calls {
                conversation.turns.push(Turn::ToolResult {
                    call_id: call.id.clone(),
                    content: step.results.get(&call.id).cloned().unwrap_or_else(|| {
                        "Not run: the user interrupted before it started.".into()
                    }),
                });
            }
        }

        let written: Vec<&str> = produced
            .steps
            .iter()
            .map(|s| s.written.trim())
            .filter(|s| !s.is_empty())
            .collect();
        reply.written = written.join(" ");
        reply.spoken = spoken_all
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        reply.actions.extend(produced.actions);
        reply.thinker = produced.thinker;
        reply.talker = produced.talker;
        reply.timing.first_text = produced.first_text;
        reply.timing.first_audio = heard.first_audio;
        reply.degraded = heard.problem.or(produced.problem);
        Ok(())
    }

    /// The producing half: thinker steps, clause rendering, tool calls.
    #[allow(clippy::too_many_lines)] // one loop, read top to bottom
    #[allow(clippy::too_many_arguments)]
    async fn produce(
        &self,
        history: Vec<Turn>,
        language: Option<&LanguageTag>,
        tx: mpsc::Sender<Rendered>,
        activity: &watch::Sender<Activity>,
        observer: &dyn Observer,
        started: Instant,
        think: bool,
        cx: &Ctx,
    ) -> Result<Produced, ConversationError> {
        let specs = self.specs();
        let instructions = self.instructions();
        let label = self.label();
        let mut produced = Produced {
            steps: Vec::new(),
            actions: Vec::new(),
            dispatched: false,
            thinker: None,
            talker: None,
            first_text: None,
            problem: None,
        };
        let mut listening = true; // playback still running
        // Something already happened this turn, e.g. an approved send.
        let acted_before = history
            .iter()
            .rev()
            .take_while(|t| !t.is_user())
            .any(|t| matches!(t, Turn::ToolResult { .. } | Turn::Event { .. }));
        let steps = if think {
            self.config.max_steps.max(1)
        } else {
            0
        };
        let mut asked = false;
        // What the user says is routed first, by a forced choice: start,
        // tell or stop a task, or just talk. Free words never decide whether
        // work happens: small models promise work they never hand on.
        let routes = self.desk().is_some() && history.last().is_some_and(Turn::is_user);

        for index in 0..steps {
            let routing = routes && index == 0;
            let mut turns = history.clone();
            for step in &produced.steps {
                push_step(&mut turns, step);
            }
            let request = ThinkRequest {
                must_use_tool: routing,
                instructions: instructions.clone(),
                turns,
                language: language.cloned(),
                tools: specs.clone(),
            };
            let (thinker, mut stream) =
                match cancellable(cx, start_thinking(&self.thinker, &request, label, cx)).await {
                    Ok(Ok(started)) => started,
                    Ok(Err(error)) if index == 0 => return Err(error),
                    Ok(Err(error)) => {
                        produced.problem = Some(format!("the thinker failed to continue: {error}"));
                        break;
                    }
                    Err(_) => break,
                };
            produced.thinker.get_or_insert(thinker);

            let mut step = Step::default();
            let mut chunker = ClauseChunker::default();
            let mut failed = false;
            // Replies speak as they stream, for the quickest answer. A
            // routing step is a decision, never heard.
            let live = !routing;
            let mut held: Vec<String> = Vec::new();
            // A promise of work ("let me find it") is held back, with all
            // that follows it, until the step ends: dropped if work follows
            // (the acknowledgement speaks for it), spoken if work is already
            // under way, and kept by code if it is empty.
            let acted = acted_before || produced.dispatched;
            let fronting = self.desk().is_some() && !routing;
            let guarded = fronting && !acted;
            let mut holding = false;
            loop {
                match cancellable(cx, stream.next()).await {
                    Ok(Some(Ok(ThinkEvent::Text(fragment)))) => {
                        produced.first_text.get_or_insert_with(|| started.elapsed());
                        step.written.push_str(&fragment);
                        for clause in chunker.push(&fragment) {
                            holding = holding
                                || (fronting && claims::front_claim(&clause) != Claim::None);
                            if !live || holding {
                                held.push(clause);
                                continue;
                            }
                            listening = listening
                                && self
                                    .send_clause(
                                        &tx,
                                        index,
                                        clause,
                                        language,
                                        label,
                                        &mut produced,
                                        cx,
                                    )
                                    .await;
                        }
                    }
                    Ok(Some(Ok(ThinkEvent::ToolCall(call)))) => step.calls.push(call),
                    Ok(Some(Err(error))) => {
                        produced.problem = Some(format!("the thinker failed mid-reply: {error}"));
                        failed = true;
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        produced.steps.push(step);
                        return Ok(produced);
                    }
                }
            }
            if !failed && let Some(clause) = chunker.finish() {
                holding = holding || (fronting && claims::front_claim(&clause) != Claim::None);
                if live && !holding {
                    listening = listening
                        && self
                            .send_clause(&tx, index, clause, language, label, &mut produced, cx)
                            .await;
                } else {
                    held.push(clause);
                }
            }
            if routing {
                step.written.clear();
            }
            let mut work: Vec<ToolCall> = Vec::with_capacity(step.calls.len());
            for call in &step.calls {
                if call.name == JUST_TALK {
                    step.results
                        .insert(call.id.clone(), "Now reply to the user in words.".into());
                } else {
                    work.push(call.clone());
                }
            }
            // Words alone that promise or claim work are kept by code: her
            // own words are handed on, and the empty promise goes unsaid.
            if !failed
                && listening
                && guarded
                && work.is_empty()
                && claims::front_claim(&step.written) != Claim::None
                && let Some(desk) = self.desk()
            {
                let words = latest_words(&history);
                if !words.is_empty() {
                    tracing::debug!(step = index, "promise without a task; handing it on");
                    let call = desk.hand_on(&words, format!("handed-on-{index}"));
                    step.calls.push(call.clone());
                    work.push(call);
                    held.clear();
                }
            }
            if !routing && (!live || holding) && work.is_empty() {
                for clause in held {
                    listening = listening
                        && self
                            .send_clause(&tx, index, clause, language, label, &mut produced, cx)
                            .await;
                }
            }
            if routing && work.is_empty() && !failed && listening {
                // Just talk: the reply follows.
                produced.steps.push(step);
                continue;
            }
            if failed || work.is_empty() || !listening {
                // A failed or silenced reply must not leave calls unanswered.
                for call in &step.calls {
                    step.results
                        .entry(call.id.clone())
                        .or_insert_with(|| "Not run.".into());
                }
                produced.steps.push(step);
                break;
            }

            if let Some(desk) = self.desk() {
                for done in self
                    .dispatch(desk, &work, activity, observer, &mut produced, cx)
                    .await
                {
                    step.results.insert(done.call_id, done.content);
                }
            }
            produced.steps.push(step);
            if cx.check().is_err() {
                break;
            }
            if let Some(summary) = self.desk().and_then(|desk| desk.ask_next()) {
                self.read_back(&summary, &tx, index + 1, language, label, &mut produced, cx)
                    .await;
                asked = true;
                break;
            }
            if index + 1 == steps {
                produced.problem = Some(format!(
                    "stopped after {} steps without finishing",
                    self.config.max_steps
                ));
            }
        }
        // A task may be waiting for an OK that nobody has asked about yet.
        if !asked
            && listening
            && cx.check().is_ok()
            && let Some(summary) = self.desk().and_then(|desk| desk.ask_next())
        {
            let index = produced.steps.len();
            self.read_back(&summary, &tx, index, language, label, &mut produced, cx)
                .await;
        }
        drop(tx);
        Ok(produced)
    }

    /// Reads back exactly what will happen, in code: the user approves what
    /// they heard, not a model's paraphrase of it.
    #[allow(clippy::too_many_arguments)]
    async fn read_back(
        &self,
        summary: &str,
        tx: &mpsc::Sender<Rendered>,
        step: usize,
        language: Option<&LanguageTag>,
        label: Sensitivity,
        produced: &mut Produced,
        cx: &Ctx,
    ) -> bool {
        let question = confirmation(summary);
        let mut chunker = ClauseChunker::default();
        let mut clauses = chunker.push(&question);
        clauses.extend(chunker.finish());
        let mut listening = true;
        for clause in clauses {
            listening = listening
                && self
                    .send_clause(tx, step, clause, language, label, produced, cx)
                    .await;
        }
        produced.steps.push(Step {
            written: question,
            ..Step::default()
        });
        listening
    }

    /// Hands the front desk's calls to the desk, narrating what the tasks
    /// are doing while the desk waits for them.
    async fn dispatch(
        &self,
        desk: &Arc<Desk>,
        calls: &[ToolCall],
        activity: &watch::Sender<Activity>,
        observer: &dyn Observer,
        produced: &mut Produced,
        cx: &Ctx,
    ) -> Vec<Dispatched> {
        produced.dispatched = true;
        activity.send_modify(|a| {
            a.serial += 1;
            a.busy = Busy::Running(None);
        });
        let mut progress = desk.progress();
        let narrate = async {
            while progress.changed().await.is_ok() {
                let now = progress.borrow_and_update().clone();
                activity.send_modify(|a| {
                    if now.is_some() {
                        a.serial += 1;
                    }
                    a.busy = Busy::Running(now);
                });
            }
        };
        let dispatched = tokio::select! {
            biased;
            dispatched = desk.dispatch(calls, cx) => dispatched,
            () = narrate => Vec::new(),
        };
        activity.send_modify(|a| a.busy = Busy::No);
        for done in &dispatched {
            record(&done.actions, &mut produced.actions, observer);
        }
        dispatched
    }

    /// Renders `clause` and queues it for playback. Returns false once
    /// playback has stopped or rendering failed: nothing more will be heard.
    #[allow(clippy::too_many_arguments)]
    async fn send_clause(
        &self,
        tx: &mpsc::Sender<Rendered>,
        step: usize,
        clause: String,
        language: Option<&LanguageTag>,
        label: Sensitivity,
        produced: &mut Produced,
        cx: &Ctx,
    ) -> bool {
        match self.render(clause, language, label, cx).await {
            Ok((id, text, audio)) => {
                produced.talker.get_or_insert(id);
                tx.send(Rendered { step, text, audio }).await.is_ok()
            }
            Err(error) => {
                produced.problem.get_or_insert(error);
                false
            }
        }
    }

    /// The consuming half: plays clauses in order, noting what was heard,
    /// and fills long silences.
    async fn consume(
        &self,
        mut rx: mpsc::Receiver<Rendered>,
        mut activity: watch::Receiver<Activity>,
        language: Option<&LanguageTag>,
        observer: &dyn Observer,
        started: Instant,
        cx: &Ctx,
    ) -> Heard {
        let mut heard = Heard {
            clauses: Vec::new(),
            first_audio: None,
            problem: None,
        };
        let mut filler = Filler {
            said_anything: false,
            quiet_since: Instant::now(),
            narrated: 0,
            still: 0,
        };
        let mut watching = self.config.pacing.is_some();
        loop {
            let wake = self.next_filler(&filler, &activity.borrow());
            let phrase = tokio::select! {
                biased;
                rendered = rx.recv() => {
                    let Some(rendered) = rendered else { break };
                    if cx.check().is_err() {
                        break;
                    }
                    heard.first_audio.get_or_insert_with(|| started.elapsed());
                    observer.on_event(ConversationEvent::Speaking(&rendered.text));
                    let played = self.playback.play(&rendered.audio, cx).await;
                    filler.said_anything = true;
                    filler.quiet_since = Instant::now();
                    match played {
                        Ok(played) if played.completed => {
                            heard.clauses.push((rendered.step, rendered.text));
                            continue;
                        }
                        Ok(played) => {
                            let part = heard_part(
                                &rendered.text,
                                played.duration,
                                rendered.audio.duration(),
                            );
                            heard.clauses.push((rendered.step, part));
                        }
                        Err(error) => heard.problem = Some(format!("playback failed: {error}")),
                    }
                    break;
                }
                changed = activity.changed(), if watching => {
                    if changed.is_err() {
                        watching = false;
                        continue;
                    }
                    // A task has started (it may already be over): acknowledge
                    // it at once.
                    let started_task = activity.borrow_and_update().serial > 0;
                    if !started_task || filler.said_anything {
                        continue;
                    }
                    self.next_phrase(|p| &p.acknowledgements)
                }
                () = sleep_until(wake), if wake.is_some() => {
                    self.filler_phrase(&mut filler, &activity.borrow())
                }
            };
            let Some(phrase) = phrase else { continue };
            filler.said_anything = true;
            if let Some(clip) = self.cue(&phrase, language, cx).await {
                if cx.check().is_err() {
                    break;
                }
                heard.first_audio.get_or_insert_with(|| started.elapsed());
                observer.on_event(ConversationEvent::Speaking(&phrase));
                match self.playback.play(&clip, cx).await {
                    Ok(played) if played.completed => {}
                    _ => break,
                }
            }
            filler.quiet_since = Instant::now();
        }
        drop(rx); // stops the producer if playback ended early
        heard
    }

    /// When silence should next be filled, if at all.
    fn next_filler(&self, filler: &Filler, activity: &Activity) -> Option<Instant> {
        let pacing = self.config.pacing.as_ref()?;
        let after = if !filler.said_anything {
            pacing.acknowledge_after
        } else if matches!(activity.busy, Busy::Running(Some(_)))
            && activity.serial != filler.narrated
        {
            pacing.narrate_after
        } else if filler.still < pacing.max_still && !pacing.still_working.is_empty() {
            pacing.still_after
        } else {
            return None;
        };
        Some(filler.quiet_since + after)
    }

    /// What to say now that the silence has lasted, if anything.
    fn filler_phrase(&self, filler: &mut Filler, activity: &Activity) -> Option<String> {
        if !filler.said_anything {
            return self.next_phrase(|p| &p.acknowledgements);
        }
        if let Busy::Running(Some(progress)) = &activity.busy
            && activity.serial != filler.narrated
        {
            filler.narrated = activity.serial;
            return Some(format!("{progress}."));
        }
        filler.still += 1;
        self.next_phrase(|p| &p.still_working)
    }

    /// The next of a set of phrases, rotating.
    fn next_phrase(&self, set: impl Fn(&Pacing) -> &Vec<String>) -> Option<String> {
        let phrases = set(self.config.pacing.as_ref()?);
        if phrases.is_empty() {
            return None;
        }
        let turn = self.cue_turn.fetch_add(1, Ordering::Relaxed);
        Some(phrases[turn % phrases.len()].clone())
    }

    /// `phrase`, rendered once and reused.
    async fn cue(
        &self,
        phrase: &str,
        language: Option<&LanguageTag>,
        cx: &Ctx,
    ) -> Option<AudioClip> {
        let cached = self
            .cues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(phrase)
            .cloned();
        if cached.is_some() {
            return cached;
        }
        // Fixed phrases carry nothing of the user's.
        match self
            .render(phrase.to_owned(), language, Sensitivity::Public, cx)
            .await
        {
            Ok((_, _, clip)) => {
                self.cues
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(phrase.to_owned(), clip.clone());
                Some(clip)
            }
            Err(error) => {
                tracing::debug!(%error, "could not render a cue");
                None
            }
        }
    }

    /// Renders one clause with the first talker that succeeds.
    async fn render(
        &self,
        text: String,
        language: Option<&LanguageTag>,
        label: Sensitivity,
        cx: &Ctx,
    ) -> Result<(ProviderId, String, AudioClip), String> {
        let request = SpeechRequest {
            text,
            language: language.cloned(),
            voice: self.config.voice.clone(),
        };
        let candidates = self.talker.candidates(label, language);
        if candidates.allowed.is_empty() {
            return Err("no talker may speak this reply".into());
        }
        let mut last = String::new();
        for talker in candidates.allowed {
            let id = talker.descriptor().id.clone();
            match cancellable(cx, talker.synthesize(&request, cx)).await {
                Ok(Ok(audio)) => return Ok((id, request.text, audio)),
                Ok(Err(error)) => {
                    tracing::warn!(provider = %id, %error, "talker failed");
                    let fall_back = error.allows_fallback();
                    last = format!("the talker failed: {error}");
                    if !fall_back {
                        break;
                    }
                }
                Err(interrupted) => return Err(interrupted.to_string()),
            }
        }
        Err(last)
    }
}

/// Reports how a reply ended.
fn finish(mut reply: Reply, observer: &dyn Observer, started: Instant, cx: &Ctx) -> Reply {
    reply.interrupted |= cx.check().is_err();
    if reply.interrupted {
        observer.on_event(ConversationEvent::Interrupted);
    }
    if let Some(problem) = &reply.degraded {
        observer.on_event(ConversationEvent::Problem(problem));
    }
    reply.timing.finished = started.elapsed();
    reply
}

/// Passes on what tasks did.
fn record(actions: &[ActionRecord], into: &mut Vec<ActionRecord>, observer: &dyn Observer) {
    for action in actions {
        observer.on_event(ConversationEvent::Action(action));
        into.push(action.clone());
    }
}

/// Starts a reply with the first thinker that succeeds.
async fn start_thinking(
    thinker_router: &Router<dyn Thinker>,
    request: &ThinkRequest,
    label: Sensitivity,
    cx: &Ctx,
) -> Result<(ProviderId, ThinkStream), ConversationError> {
    let candidates = thinker_router.candidates(label, request.language.as_ref());
    if candidates.allowed.is_empty() {
        return Err(ConversationError::NoThinker {
            excluded: candidates.excluded,
        });
    }
    let mut failures = Vec::new();
    for thinker in candidates.allowed {
        let id = thinker.descriptor().id.clone();
        match thinker.think(request, cx).await {
            Ok(stream) => return Ok((id, stream)),
            Err(ProviderError::Interrupted(interrupted)) => return Err(interrupted.into()),
            Err(error) => {
                tracing::warn!(provider = %id, %error, "thinker failed to start");
                let fall_back = error.allows_fallback();
                failures.push((id, error));
                if !fall_back {
                    break;
                }
            }
        }
    }
    Err(ConversationError::ThinkFailed { failures })
}

/// Sleeps until `at`, or forever if there is no `at`.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Appends a finished step to a request's turns: what was written, the calls
/// and their results.
fn push_step(turns: &mut Vec<Turn>, step: &Step) {
    turns.push(Turn::Assistant {
        text: step.written.trim().to_owned(),
        tool_calls: step.calls.clone(),
    });
    for call in &step.calls {
        turns.push(Turn::ToolResult {
            call_id: call.id.clone(),
            content: step.results.get(&call.id).cloned().unwrap_or_default(),
        });
    }
}

/// The words of `text` heard in `played` of `total` audio, assuming an even
/// speaking rate. Rounded down: a half-heard word was not heard.
fn heard_part(text: &str, played: Duration, total: Duration) -> String {
    if total.is_zero() {
        return String::new();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let fraction = (played.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let count = (words.len() as f64 * fraction).floor() as usize;
    words[..count].join(" ")
}

/// What the user said since the assistant last spoke, in their words.
fn latest_words(history: &[Turn]) -> String {
    let mut words: Vec<&str> = history
        .iter()
        .rev()
        .take_while(|t| !matches!(t, Turn::Assistant { .. }))
        .filter_map(|t| match t {
            Turn::User { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    words.reverse();
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heard_part_rounds_down_to_whole_words() {
        let text = "one two three four";
        let total = Duration::from_secs(4);
        assert_eq!(
            heard_part(text, Duration::from_millis(2900), total),
            "one two"
        );
        assert_eq!(heard_part(text, Duration::ZERO, total), "");
        assert_eq!(heard_part(text, total, total), text);
        assert_eq!(heard_part(text, total, Duration::ZERO), "");
    }

    #[test]
    fn history_merges_repeated_user_turns_and_starts_with_the_user() {
        let mut c = Conversation::default();
        c.push_user("what time");
        c.push_user("is it in Tokyo");
        c.turns.push(Turn::assistant("It's 9 PM."));
        c.push_user("thanks");
        assert_eq!(
            c.turns(),
            [
                Turn::user("what time is it in Tokyo"),
                Turn::assistant("It's 9 PM."),
                Turn::user("thanks")
            ]
        );
        assert_eq!(c.recent(2), [Turn::user("thanks")]);
        assert_eq!(c.recent(10).len(), 3);
    }

    #[test]
    fn history_never_starts_with_a_tool_result() {
        let mut c = Conversation::default();
        c.push_user("open notes");
        c.turns.push(Turn::Assistant {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "open_app".into(),
                arguments: "{}".into(),
            }],
        });
        c.turns.push(Turn::ToolResult {
            call_id: "1".into(),
            content: "ok".into(),
        });
        c.turns.push(Turn::assistant("Done."));
        assert_eq!(c.recent(2).len(), 4, "reaches back to the user turn");
        assert_eq!(c.recent(4).len(), 4);
    }
}
