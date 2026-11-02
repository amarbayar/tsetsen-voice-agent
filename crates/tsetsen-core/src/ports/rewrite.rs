use async_trait::async_trait;

use crate::{AppId, Ctx, Describe, LanguageTag, ProviderError};

/// A raw transcript to clean up.
///
/// It deliberately carries no screen content (window titles, field values), so
/// the whole request is [`crate::Sensitivity::Personal`] and policy can treat it
/// as such.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RewriteRequest {
    /// The transcript exactly as recognized.
    pub transcript: String,
    /// The app the text is going into, for tone and formatting.
    pub app: Option<AppId>,
    /// Names and jargon to preserve verbatim.
    pub vocabulary: Vec<String>,
    /// Language of the transcript, if known.
    pub language: Option<LanguageTag>,
}

/// The cleaned-up text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewrite {
    /// Text ready to insert.
    pub text: String,
}

/// Transcript cleanup: removes fillers, resolves self-corrections
/// ("3, no wait, 4pm" → "4pm"), and fixes punctuation and formatting.
///
/// A rewriter must only rewrite. It must never answer, summarize or add content.
/// The engine enforces this with a guard, so adapters cannot opt out.
#[async_trait]
pub trait Rewriter: Describe + Send + Sync {
    /// Cleans up one transcript.
    async fn rewrite(&self, request: &RewriteRequest, cx: &Ctx) -> Result<Rewrite, ProviderError>;
}
