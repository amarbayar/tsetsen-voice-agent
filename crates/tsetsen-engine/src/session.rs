//! A live voice session: listens, replies, and lets the user interrupt.
//!
//! Microphone frames arrive continuously, already echo-cancelled by the audio
//! adapter. Between replies, the endpointer waits for the user to finish a
//! turn. During a reply it switches to a stricter barge-in setting; if the
//! user talks over the reply, the reply is cancelled and the same speech
//! becomes their next turn, first syllables included.
//!
//! Tasks keep working between turns. When one has news and the user is
//! quiet, the session lets the assistant speak up; that can be interrupted
//! like any reply.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;
use tsetsen_core::ports::{AudioClip, VoiceActivity};
use tsetsen_core::{Cancellation, Ctx, ProviderError, RequestId};

use crate::conversation::{
    Conversation, ConversationEngine, ConversationError, ConversationEvent, Observer, TurnOutcome,
};
use crate::turn::{EndpointConfig, Endpointer, TurnEvent};

/// What a session reports after each turn.
pub type TurnResult = Result<TurnOutcome, ConversationError>;

/// Thresholds for the two listening modes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionConfig {
    /// While waiting for the user's turn.
    pub listening: EndpointConfig,
    /// While the assistant is replying.
    pub barge_in: EndpointConfig,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            listening: EndpointConfig::listening(),
            barge_in: EndpointConfig::barge_in(),
        }
    }
}

/// The session could not continue.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Voice activity detection failed.
    #[error("voice activity detection failed: {0}")]
    Vad(#[from] ProviderError),
    /// The detector reported a zero sample rate or frame length.
    #[error("voice activity detector reported a zero sample rate or frame length")]
    InvalidDetector,
}

/// A reply under way: the conversation it updates, and how the turn ended
/// (`None` if it turned out there was nothing to say).
type Running<'a> = Pin<Box<dyn Future<Output = (Conversation, Option<TurnResult>)> + Send + 'a>>;

/// Starts a reply: to what the user said, or with news if they said
/// nothing.
fn start<'a>(
    engine: &'a ConversationEngine,
    mut history: Conversation,
    audio: Option<AudioClip>,
    observer: &'a dyn Observer,
    request: u64,
) -> (Running<'a>, Cancellation) {
    let cancel = Cancellation::new();
    let cx = Ctx::new(RequestId::new(request)).with_cancellation(cancel.clone());
    let reply: Running<'a> = Box::pin(async move {
        let outcome = match audio {
            Some(audio) => Some(engine.respond(&mut history, &audio, observer, &cx).await),
            None => match engine.announce(&mut history, observer, &cx).await {
                Ok(Some(reply)) => Some(Ok(TurnOutcome::Announced(reply))),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            },
        };
        (history, outcome)
    });
    (reply, cancel)
}

/// Runs a conversation over a live microphone stream until `frames` closes or
/// `stop` is cancelled. Calls `on_turn` after every turn.
pub async fn run_session(
    engine: &ConversationEngine,
    vad: &mut dyn VoiceActivity,
    mut frames: mpsc::Receiver<Vec<f32>>,
    observer: &dyn Observer,
    config: SessionConfig,
    stop: &Cancellation,
    mut on_turn: impl FnMut(&TurnResult) + Send,
) -> Result<Conversation, SessionError> {
    let rate = vad.sample_rate_hz();
    let frame_len = vad.frame_len();
    if rate == 0 || frame_len == 0 {
        return Err(SessionError::InvalidDetector);
    }
    let mut endpointer = Endpointer::new(config.listening, rate, frame_len);
    let mut pending: Vec<f32> = Vec::with_capacity(frame_len * 2);
    let mut conversation = Conversation::default();
    let mut running: Option<(Running<'_>, Cancellation)> = None;
    let mut queued: Option<Vec<f32>> = None;
    let mut request = 0u64;
    let mut news = engine.news();

    loop {
        if stop.is_cancelled() {
            break;
        }
        // Start the next turn once the previous reply has fully stopped.
        if running.is_none()
            && let Some(samples) = queued.take()
        {
            let audio = AudioClip::new(rate, samples).map_err(|_| SessionError::InvalidDetector)?;
            request += 1;
            let history = std::mem::take(&mut conversation);
            running = Some(start(engine, history, Some(audio), observer, request));
            endpointer.set_config(config.barge_in);
        }
        // Speak up with news when the user is quiet.
        if running.is_none() && queued.is_none() && !endpointer.in_speech() && engine.has_news() {
            request += 1;
            let history = std::mem::take(&mut conversation);
            running = Some(start(engine, history, None, observer, request));
            endpointer.set_config(config.barge_in);
        }

        let frame = if let Some((reply, _)) = running.as_mut() {
            tokio::select! {
                (history, result) = reply => {
                    conversation = history;
                    running = None;
                    endpointer.set_config(config.listening);
                    if let Some(result) = result {
                        if let Err(error) = &result {
                            observer.on_event(ConversationEvent::Problem(&error.to_string()));
                        }
                        on_turn(&result);
                    }
                    continue;
                }
                frame = frames.recv() => frame,
            }
        } else if let Some(news) = news.as_mut() {
            tokio::select! {
                frame = frames.recv() => frame,
                // Something to tell the user: back to the top of the loop.
                changed = news.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    continue;
                }
            }
        } else {
            frames.recv().await
        };
        let Some(frame) = frame else { break };

        pending.extend_from_slice(&frame);
        let mut offset = 0;
        while pending.len() - offset >= frame_len {
            let chunk = &pending[offset..offset + frame_len];
            offset += frame_len;
            let probability = vad.speech_probability(chunk)?;
            match endpointer.push(chunk, probability) {
                Some(TurnEvent::SpeechStarted) => {
                    observer.on_event(ConversationEvent::Listening);
                    if let Some((_, cancel)) = &running {
                        // Barge-in: stop the reply, keep listening to this turn.
                        cancel.cancel();
                        endpointer.set_config(config.listening);
                    }
                }
                Some(TurnEvent::Utterance(samples)) => {
                    // Merge with a turn still waiting for the previous reply to stop.
                    match &mut queued {
                        Some(earlier) => earlier.extend(samples),
                        None => queued = Some(samples),
                    }
                }
                None => {}
            }
        }
        pending.drain(..offset);
    }

    if let Some((reply, cancel)) = running.take() {
        cancel.cancel();
        let (history, result) = reply.await;
        conversation = history;
        if let Some(result) = result {
            on_turn(&result);
        }
    }
    Ok(conversation)
}
