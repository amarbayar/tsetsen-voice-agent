//! Scriptable fakes for Tsetsen's ports.
//!
//! Each fake takes a closure that decides its response, and records how it
//! was called, so tests state behavior directly instead of mocking frameworks.

mod personal;

pub use personal::{
    FakeContacts, FakeFiles, FakeLauncher, FakeMailer, FakeMemory, FakePhotos, FakeShowcase,
    contact, file_at, photo_at,
};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use tsetsen_core::ports::{
    ActionReport, AudioClip, Delivery, Desktop, Rewrite, RewriteRequest, Rewriter, SpeechToText,
    SttHints, TextEntry, ThinkEvent, ThinkStream, ToolCall, Transcript, UiAction, UiNode,
    WindowInfo, WindowSnapshot,
};
use tsetsen_core::{
    Ctx, Describe, DesktopError, ElementRef, Layer, Locality, ProviderDescriptor, ProviderError,
    WindowId,
};

/// A descriptor for a provider running on this device.
#[must_use]
pub fn local(id: &str, layer: Layer) -> ProviderDescriptor {
    ProviderDescriptor::new(id, layer, Locality::Local)
}

/// A descriptor for a provider running at `host`.
#[must_use]
pub fn remote(id: &str, layer: Layer, host: &str) -> ProviderDescriptor {
    ProviderDescriptor::new(id, layer, Locality::Remote { host: host.into() })
}

/// One second of 16 kHz silence. The fakes never look at the samples.
///
/// # Panics
///
/// Never: 16 kHz is a valid sample rate.
#[must_use]
pub fn one_second_of_audio() -> AudioClip {
    AudioClip::new(16_000, vec![0.0; 16_000]).expect("16 kHz is a valid sample rate")
}

type SttFn = dyn Fn(&AudioClip, &SttHints) -> Result<Transcript, ProviderError> + Send + Sync;

/// A [`SpeechToText`] whose response is a closure.
pub struct FakeStt {
    descriptor: ProviderDescriptor,
    respond: Box<SttFn>,
    calls: AtomicUsize,
}

impl FakeStt {
    /// Responds with whatever `respond` returns.
    pub fn new(
        descriptor: ProviderDescriptor,
        respond: impl Fn(&AudioClip, &SttHints) -> Result<Transcript, ProviderError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            descriptor,
            respond: Box::new(respond),
            calls: AtomicUsize::new(0),
        }
    }

    /// Always hears `text`.
    #[must_use]
    pub fn hearing(descriptor: ProviderDescriptor, text: &str) -> Self {
        let text = text.to_owned();
        Self::new(descriptor, move |_, _| Ok(Transcript::new(text.clone())))
    }

    /// Hears each of `texts` in turn, one per call; the last repeats.
    #[must_use]
    pub fn hearing_in_turn(descriptor: ProviderDescriptor, texts: &[&str]) -> Self {
        let texts: Vec<String> = texts.iter().map(|t| (*t).to_owned()).collect();
        let next = AtomicUsize::new(0);
        Self::new(descriptor, move |_, _| {
            let i = next.fetch_add(1, Ordering::SeqCst);
            let text = texts.get(i).or(texts.last()).cloned().unwrap_or_default();
            Ok(Transcript::new(text))
        })
    }

    /// Always fails with the error `error` builds.
    pub fn failing(
        descriptor: ProviderDescriptor,
        error: impl Fn() -> ProviderError + Send + Sync + 'static,
    ) -> Self {
        Self::new(descriptor, move |_, _| Err(error()))
    }

    /// How many times it was called.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Describe for FakeStt {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

#[async_trait]
impl SpeechToText for FakeStt {
    async fn transcribe(
        &self,
        audio: &AudioClip,
        hints: &SttHints,
        _cx: &Ctx,
    ) -> Result<Transcript, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.respond)(audio, hints)
    }
}

type RewriteFn = dyn Fn(&RewriteRequest) -> Result<Rewrite, ProviderError> + Send + Sync;

/// A [`Rewriter`] whose response is a closure, optionally delayed.
pub struct FakeRewriter {
    descriptor: ProviderDescriptor,
    respond: Box<RewriteFn>,
    delay: Option<Duration>,
    requests: Mutex<Vec<RewriteRequest>>,
}

impl FakeRewriter {
    /// Responds with whatever `respond` returns.
    pub fn new(
        descriptor: ProviderDescriptor,
        respond: impl Fn(&RewriteRequest) -> Result<Rewrite, ProviderError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            descriptor,
            respond: Box::new(respond),
            delay: None,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Always rewrites to `text`.
    #[must_use]
    pub fn returning(descriptor: ProviderDescriptor, text: &str) -> Self {
        let text = text.to_owned();
        Self::new(descriptor, move |_| Ok(Rewrite { text: text.clone() }))
    }

    /// Waits `delay` (on the Tokio clock) before responding.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Every request received, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<RewriteRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Describe for FakeRewriter {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

#[async_trait]
impl Rewriter for FakeRewriter {
    async fn rewrite(&self, request: &RewriteRequest, _cx: &Ctx) -> Result<Rewrite, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        (self.respond)(request)
    }
}

type ActFn = dyn Fn(&WindowId, &UiAction) -> Result<ActionReport, DesktopError> + Send + Sync;

/// A window on the fake screen.
#[derive(Clone, Debug)]
pub struct FakeWindow {
    /// What [`Desktop::windows`] reports.
    pub info: WindowInfo,
    /// What [`Desktop::snapshot`] reports.
    pub root: UiNode,
}

#[derive(Default)]
struct Screen {
    installed: Vec<String>,
    windows: Vec<FakeWindow>,
    opened: Vec<String>,
    next_window: usize,
}

impl Screen {
    /// Adds a focused window in front of the others.
    fn add_window(&mut self, app: &str, title: &str, root: UiNode) -> WindowInfo {
        self.next_window += 1;
        let info = WindowInfo {
            id: WindowId::new(format!("w-{}", self.next_window)),
            app: app.to_owned(),
            title: title.to_owned(),
            focused: true,
            minimized: false,
        };
        self.bring_forward(FakeWindow {
            info: info.clone(),
            root,
        });
        info
    }

    fn bring_forward(&mut self, mut window: FakeWindow) {
        for other in &mut self.windows {
            other.info.focused = false;
        }
        window.info.focused = true;
        self.windows.insert(0, window);
    }
}

/// A [`Desktop`] with a scriptable screen. Records every action and responds
/// to it with a closure; typed text appears in the target field.
pub struct FakeDesktop {
    respond: Box<ActFn>,
    actions: Mutex<Vec<(WindowId, UiAction)>>,
    screen: Mutex<Screen>,
    text_entry: TextEntry,
}

impl FakeDesktop {
    /// Responds with whatever `respond` returns.
    pub fn new(
        respond: impl Fn(&WindowId, &UiAction) -> Result<ActionReport, DesktopError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            respond: Box::new(respond),
            actions: Mutex::new(Vec::new()),
            screen: Mutex::new(Screen::default()),
            text_entry: TextEntry::default(),
        }
    }

    /// Reports `text_entry` as how it types.
    #[must_use]
    pub fn with_text_entry(mut self, text_entry: TextEntry) -> Self {
        self.text_entry = text_entry;
        self
    }

    /// Every action succeeds and is verified.
    #[must_use]
    pub fn verified() -> Self {
        Self::new(|_, _| {
            Ok(ActionReport {
                delivery: Delivery::Verified,
            })
        })
    }

    /// Apps that [`Desktop::open_app`] can open.
    #[must_use]
    pub fn with_installed(self, apps: &[&str]) -> Self {
        self.screen().installed = apps.iter().map(|a| (*a).to_owned()).collect();
        self
    }

    /// Puts a window on screen, in front of the others.
    #[must_use]
    pub fn with_window(self, app: &str, title: &str, root: UiNode) -> Self {
        self.screen().add_window(app, title, root);
        self
    }

    /// The windows on screen, front first.
    #[must_use]
    pub fn windows_now(&self) -> Vec<FakeWindow> {
        self.screen().windows.clone()
    }

    /// Every action received, in order.
    #[must_use]
    pub fn actions(&self) -> Vec<(WindowId, UiAction)> {
        self.actions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every app opened, in order.
    #[must_use]
    pub fn opened(&self) -> Vec<String> {
        self.screen().opened.clone()
    }

    fn screen(&self) -> std::sync::MutexGuard<'_, Screen> {
        self.screen.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A new, empty document: a focused text area.
#[must_use]
pub fn blank_document() -> UiNode {
    UiNode {
        role: "window".into(),
        children: vec![UiNode {
            element: Some(ElementRef::new("doc")),
            role: "textfield".into(),
            value: Some(String::new()),
            states: vec!["focused".into()],
            actions: vec!["TypeText".into(), "SetValue".into()],
            ..UiNode::default()
        }],
        ..UiNode::default()
    }
}

fn find_mut<'a>(node: &'a mut UiNode, element: &ElementRef) -> Option<&'a mut UiNode> {
    if node.element.as_ref() == Some(element) {
        return Some(node);
    }
    node.children.iter_mut().find_map(|c| find_mut(c, element))
}

#[async_trait]
impl Desktop for FakeDesktop {
    fn text_entry(&self) -> TextEntry {
        self.text_entry
    }

    async fn installed_apps(&self, _cx: &Ctx) -> Result<Vec<String>, DesktopError> {
        Ok(self.screen().installed.clone())
    }

    async fn open_app(&self, app: &str, _cx: &Ctx) -> Result<WindowInfo, DesktopError> {
        let mut screen = self.screen();
        if !screen.installed.iter().any(|a| a == app) {
            return Err(DesktopError::AppNotFound(app.to_owned()));
        }
        screen.opened.push(app.to_owned());
        if let Some(i) = screen.windows.iter().position(|w| w.info.app == app) {
            let window = screen.windows.remove(i);
            screen.bring_forward(window);
            return Ok(screen.windows[0].info.clone());
        }
        Ok(screen.add_window(app, "Untitled", blank_document()))
    }

    async fn windows(&self, _cx: &Ctx) -> Result<Vec<WindowInfo>, DesktopError> {
        Ok(self
            .screen()
            .windows
            .iter()
            .map(|w| w.info.clone())
            .collect())
    }

    async fn snapshot(&self, window: &WindowId, _cx: &Ctx) -> Result<WindowSnapshot, DesktopError> {
        self.screen()
            .windows
            .iter()
            .find(|w| &w.info.id == window)
            .map(|w| WindowSnapshot {
                window: w.info.clone(),
                root: w.root.clone(),
                complete: true,
            })
            .ok_or_else(|| DesktopError::WindowNotFound(window.clone()))
    }

    async fn act(
        &self,
        window: &WindowId,
        action: &UiAction,
        _cx: &Ctx,
    ) -> Result<ActionReport, DesktopError> {
        self.actions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((window.clone(), action.clone()));
        let report = (self.respond)(window, action)?;
        if let UiAction::InsertText { element, text } = action {
            let mut screen = self.screen();
            if let Some(node) = screen
                .windows
                .iter_mut()
                .find(|w| &w.info.id == window)
                .and_then(|w| find_mut(&mut w.root, element))
            {
                node.value.get_or_insert_with(String::new).push_str(text);
            }
        }
        Ok(report)
    }
}

/// One scripted reply: events (or a mid-stream failure message), or an error
/// raised before streaming starts.
type ScriptedReply = Result<Vec<Result<ThinkEvent, String>>, fn() -> ProviderError>;

/// A tool call event, for scripting [`FakeThinker`].
#[must_use]
pub fn tool_call(id: &str, name: &str, arguments: &str) -> ThinkEvent {
    ThinkEvent::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    })
}

/// A [`Thinker`] that streams scripted replies, optionally paced. Each call
/// takes the next reply; the last one repeats.
///
/// [`Thinker`]: tsetsen_core::ports::Thinker
pub struct FakeThinker {
    descriptor: ProviderDescriptor,
    replies: Vec<ScriptedReply>,
    pace: Duration,
    requests: Mutex<Vec<tsetsen_core::ports::ThinkRequest>>,
}

impl FakeThinker {
    fn with_replies(descriptor: ProviderDescriptor, replies: Vec<ScriptedReply>) -> Self {
        Self {
            descriptor,
            replies,
            pace: Duration::ZERO,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Streams `fragments`, one every `pace` (on the Tokio clock).
    #[must_use]
    pub fn streaming(descriptor: ProviderDescriptor, fragments: &[&str], pace: Duration) -> Self {
        let reply = fragments
            .iter()
            .map(|f| Ok(ThinkEvent::Text((*f).to_owned())))
            .collect();
        Self {
            pace,
            ..Self::with_replies(descriptor, vec![Ok(reply)])
        }
    }

    /// Plays `replies` in order, one per request.
    #[must_use]
    pub fn scripted(descriptor: ProviderDescriptor, replies: Vec<Vec<ThinkEvent>>) -> Self {
        let replies = replies
            .into_iter()
            .map(|events| Ok(events.into_iter().map(Ok).collect()))
            .collect();
        Self::with_replies(descriptor, replies)
    }

    /// Waits `pace` before each event (on the Tokio clock).
    #[must_use]
    pub fn paced(mut self, pace: Duration) -> Self {
        self.pace = pace;
        self
    }

    /// Streams `fragments`, then fails mid-reply.
    #[must_use]
    pub fn failing_after(descriptor: ProviderDescriptor, fragments: &[&str]) -> Self {
        let mut script: Vec<Result<ThinkEvent, String>> = fragments
            .iter()
            .map(|f| Ok(ThinkEvent::Text((*f).to_owned())))
            .collect();
        script.push(Err("connection reset".into()));
        Self::with_replies(descriptor, vec![Ok(script)])
    }

    /// Fails to start, with the error `error` returns.
    #[must_use]
    pub fn failing(descriptor: ProviderDescriptor, error: fn() -> ProviderError) -> Self {
        Self::with_replies(descriptor, vec![Err(error)])
    }

    /// Every request received, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<tsetsen_core::ports::ThinkRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Describe for FakeThinker {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

#[async_trait]
impl tsetsen_core::ports::Thinker for FakeThinker {
    async fn think(
        &self,
        request: &tsetsen_core::ports::ThinkRequest,
        _cx: &Ctx,
    ) -> Result<ThinkStream, ProviderError> {
        let index = {
            let mut requests = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
            requests.push(request.clone());
            requests.len() - 1
        };
        let reply = self
            .replies
            .get(index)
            .or(self.replies.last())
            .cloned()
            .unwrap_or_else(|| Ok(Vec::new()));
        let script = reply.map_err(|error| error())?;
        let pace = self.pace;
        let stream = futures_util::stream::iter(script).then(move |item| async move {
            if !pace.is_zero() {
                tokio::time::sleep(pace).await;
            }
            item.map_err(ProviderError::Unavailable)
        });
        Ok(Box::pin(stream))
    }
}

/// A [`TextToSpeech`] that renders silence, 50 ms per character, at 16 kHz.
///
/// [`TextToSpeech`]: tsetsen_core::ports::TextToSpeech
pub struct FakeTts {
    descriptor: ProviderDescriptor,
    delay: Duration,
    fail: bool,
    requests: Mutex<Vec<tsetsen_core::ports::SpeechRequest>>,
}

/// Audio length per character of text rendered by [`FakeTts`].
pub const FAKE_SPEECH_PER_CHAR: Duration = Duration::from_millis(50);

impl FakeTts {
    /// Renders every request after `delay` (on the Tokio clock).
    #[must_use]
    pub fn new(descriptor: ProviderDescriptor, delay: Duration) -> Self {
        Self {
            descriptor,
            delay,
            fail: false,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Fails every request with a fallback-allowing error.
    #[must_use]
    pub fn failing(descriptor: ProviderDescriptor) -> Self {
        Self {
            fail: true,
            ..Self::new(descriptor, Duration::ZERO)
        }
    }

    /// The text of every request received, in order.
    #[must_use]
    pub fn texts(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|r| r.text.clone())
            .collect()
    }
}

impl Describe for FakeTts {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

#[async_trait]
impl tsetsen_core::ports::TextToSpeech for FakeTts {
    async fn synthesize(
        &self,
        request: &tsetsen_core::ports::SpeechRequest,
        _cx: &Ctx,
    ) -> Result<AudioClip, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.fail {
            return Err(ProviderError::Unavailable("speaker model crashed".into()));
        }
        let millis = FAKE_SPEECH_PER_CHAR.as_millis() * request.text.chars().count() as u128;
        let samples = usize::try_from(millis * 16).unwrap_or(usize::MAX);
        AudioClip::new(16_000, vec![0.0; samples])
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))
    }
}

/// A [`Playback`] that "plays" in real time on the Tokio clock, stops when
/// cancelled, and records what was played.
///
/// [`Playback`]: tsetsen_core::ports::Playback
#[derive(Default)]
pub struct FakePlayback {
    played: Mutex<Vec<(Duration, tsetsen_core::ports::Played)>>,
}

impl FakePlayback {
    /// A speaker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Each clip's length and how much of it played, in order.
    #[must_use]
    pub fn played(&self) -> Vec<(Duration, tsetsen_core::ports::Played)> {
        self.played
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl tsetsen_core::ports::Playback for FakePlayback {
    async fn play(
        &self,
        clip: &AudioClip,
        cx: &Ctx,
    ) -> Result<tsetsen_core::ports::Played, tsetsen_core::AudioError> {
        const STEP: Duration = Duration::from_millis(10);
        let total = clip.duration();
        let mut elapsed = Duration::ZERO;
        while elapsed < total && cx.check().is_ok() {
            let step = STEP.min(total.saturating_sub(elapsed));
            tokio::time::sleep(step).await;
            elapsed += step;
        }
        let played = tsetsen_core::ports::Played {
            duration: elapsed,
            completed: elapsed >= total,
        };
        self.played
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((total, played));
        Ok(played)
    }
}

/// A [`VoiceActivity`] that reads each frame's speech probability from its
/// first sample, so tests script speech as audio.
///
/// [`VoiceActivity`]: tsetsen_core::ports::VoiceActivity
#[derive(Clone, Copy, Debug)]
pub struct ScriptedVad {
    /// Frame length in samples.
    pub frame_len: usize,
}

impl tsetsen_core::ports::VoiceActivity for ScriptedVad {
    fn sample_rate_hz(&self) -> u32 {
        16_000
    }

    fn frame_len(&self) -> usize {
        self.frame_len
    }

    fn speech_probability(&mut self, frame: &[f32]) -> Result<f32, ProviderError> {
        Ok(frame.first().copied().unwrap_or(0.0).clamp(0.0, 1.0))
    }

    fn reset(&mut self) {}
}
