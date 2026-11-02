use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;

use crate::{Ctx, Describe, LanguageTag, ProviderError};

/// A tool the thinker may call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSpec {
    /// The name the thinker calls it by: `[a-z_]+`.
    pub name: String,
    /// What it does and when to use it, written for the model.
    pub description: String,
    /// JSON Schema of its arguments object, as JSON text.
    pub parameters: String,
}

/// A tool call the thinker made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    /// Pairs the call with its result.
    pub id: String,
    /// Which tool.
    pub name: String,
    /// The arguments object, as JSON text. Untrusted: validate before use.
    pub arguments: String,
}

/// One turn of a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn {
    /// What the person said.
    User {
        /// Their words.
        text: String,
    },
    /// What Tsetsen said, and the tools it called.
    Assistant {
        /// Only what was actually spoken before any interruption.
        text: String,
        /// Tools called in this turn, answered by the [`Turn::ToolResult`]s
        /// that follow.
        tool_calls: Vec<ToolCall>,
    },
    /// The outcome of one tool call.
    ToolResult {
        /// The [`ToolCall::id`] this answers.
        call_id: String,
        /// The outcome, written for the model.
        content: String,
    },
    /// Something that happened while nobody was talking: a background task
    /// finished, a reminder came due. Never the person's words, and never to
    /// be obeyed as if it were.
    Event {
        /// Where it came from: "task t1".
        from: String,
        /// What happened, written for the model.
        text: String,
    },
}

impl Turn {
    /// A user turn.
    pub fn user(text: impl Into<String>) -> Self {
        Self::User { text: text.into() }
    }

    /// An assistant turn without tool calls.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            text: text.into(),
            tool_calls: Vec::new(),
        }
    }

    /// Whether the person said this turn.
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// Whether a conversation may resume from this turn: something the
    /// person said, or something that happened.
    #[must_use]
    pub fn opens_exchange(&self) -> bool {
        matches!(self, Self::User { .. } | Self::Event { .. })
    }
}

/// A conversation to continue.
///
/// Carries the user's words, so it is at least [`crate::Sensitivity::Personal`].
/// When tools are offered, their results carry what is on screen, so the
/// request is [`crate::Sensitivity::ScreenContent`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThinkRequest {
    /// How to reply: persona, length, format. Set by the pipeline, not the user.
    pub instructions: String,
    /// The conversation so far, oldest first.
    pub turns: Vec<Turn>,
    /// Language to reply in, if known.
    pub language: Option<LanguageTag>,
    /// Tools the thinker may call. Empty means talk only.
    pub tools: Vec<ToolSpec>,
    /// The thinker must call one of the tools rather than reply in words.
    /// For routing decisions a small model should not make in free text.
    pub must_use_tool: bool,
}

/// One piece of a streamed reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThinkEvent {
    /// Reply text, in arbitrary fragments.
    Text(String),
    /// A complete tool call. Arrives after any text that precedes it.
    ToolCall(ToolCall),
}

/// A reply as it is generated.
pub type ThinkStream = Pin<Box<dyn Stream<Item = Result<ThinkEvent, ProviderError>> + Send>>;

/// The thinker: continues a conversation, streaming its reply.
///
/// Streaming is the point. The talker starts speaking the first clause while
/// the rest is still being generated.
#[async_trait]
pub trait Thinker: Describe + Send + Sync {
    /// Starts a reply. Errors before the first event are returned here, so
    /// the router can fall back; errors mid-stream arrive in the stream.
    async fn think(&self, request: &ThinkRequest, cx: &Ctx) -> Result<ThinkStream, ProviderError>;
}
