//! Dictation: speech in, clean text inserted into the field the user was in.

use std::sync::Arc;
use std::time::Duration;

use tsetsen_core::ports::{
    Anchor, AudioClip, Delivery, Desktop, RewriteRequest, Rewriter, SpeechToText, SttHints,
    Transcript, UiAction,
};
use tsetsen_core::{Ctx, DesktopError, Interrupted, LanguageTag, ProviderError, ProviderId};
use tsetsen_router::{Exclusion, Router};

use crate::guard::{OverlapGuard, RewriteCheck, RewriteGuard};
use crate::stt::TranscribeError;

use crate::stt::TRANSCRIPT_LABEL;

/// How long cleanup may take before the raw transcript is used instead.
///
/// It grows with the utterance because generation time does: measured with
/// Qwen3.5-9B on an M5 Max, a short phrase cleans up in about 160 ms while a
/// 40-word ramble needs about 700 ms. A fixed budget either makes short
/// dictations wait for nothing or drops cleanup on long ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewriteBudget {
    /// Allowed for any utterance.
    pub base: Duration,
    /// Added per word of transcript.
    pub per_word: Duration,
    /// Never exceeded, however long the utterance.
    pub max: Duration,
}

impl RewriteBudget {
    /// The same budget regardless of length.
    #[must_use]
    pub const fn fixed(budget: Duration) -> Self {
        Self {
            base: budget,
            per_word: Duration::ZERO,
            max: budget,
        }
    }

    /// The budget for a transcript of `words` words.
    #[must_use]
    pub fn for_words(&self, words: usize) -> Duration {
        let words = u32::try_from(words).unwrap_or(u32::MAX);
        self.base
            .saturating_add(self.per_word.saturating_mul(words))
            .min(self.max)
    }
}

impl Default for RewriteBudget {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(400),
            per_word: Duration::from_millis(15),
            max: Duration::from_millis(1500),
        }
    }
}

/// Settings for the dictation pipeline.
#[derive(Clone)]
pub struct DictationConfig {
    /// How long cleanup may take.
    pub rewrite_budget: RewriteBudget,
    /// Accepts or rejects cleanup results.
    pub guard: Arc<dyn RewriteGuard>,
}

impl Default for DictationConfig {
    fn default() -> Self {
        Self {
            rewrite_budget: RewriteBudget::default(),
            guard: Arc::new(OverlapGuard::default()),
        }
    }
}

/// One dictation: what the user said and where they were when they said it.
#[derive(Clone, Debug)]
pub struct DictationRequest {
    /// The field the user was in at hotkey press.
    pub anchor: Anchor,
    /// The utterance.
    pub audio: AudioClip,
    /// Recognition hints, also used as cleanup vocabulary.
    pub hints: SttHints,
}

/// What happened to the cleanup step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewriteStatus {
    /// The provider's cleanup was used.
    Applied(ProviderId),
    /// No rewriter is configured.
    Disabled,
    /// Rewriters exist, but none may receive this request (policy or language).
    NoEligibleProvider,
    /// The provider was too slow; the raw transcript was used.
    TimedOut(ProviderId),
    /// The provider failed; the raw transcript was used.
    Failed(ProviderId),
    /// The guard rejected the cleanup; the raw transcript was used.
    Rejected {
        /// The provider whose output was rejected.
        provider: ProviderId,
        /// What it produced, so the UI can offer it and evals can inspect it.
        /// Lives only in the outcome; it is never logged.
        candidate: String,
    },
}

/// Which providers produced the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// The speech-to-text provider that succeeded.
    pub stt: ProviderId,
    /// What happened during cleanup.
    pub rewrite: RewriteStatus,
}

/// Why text was held back instead of inserted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldReason {
    /// The user was not in a text field.
    NoTargetElement,
    /// The window or field is gone.
    TargetGone,
    /// Inserting needs the foreground, and the pipeline never takes it silently.
    NeedsForeground,
    /// The platform refused the insert.
    InsertFailed(String),
}

/// The result of a dictation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DictationOutcome {
    /// The text is in the anchored field.
    Inserted {
        /// The inserted text.
        text: String,
        /// Whether the adapter read it back.
        verified: bool,
        /// Where it came from.
        provenance: Provenance,
    },
    /// The text could not be inserted safely. It is returned so the UI can show
    /// or copy it. It is never pasted into whatever happens to be focused.
    Held {
        /// The text that was not inserted.
        text: String,
        /// Why.
        reason: HoldReason,
        /// Where it came from.
        provenance: Provenance,
    },
    /// The utterance contained no words.
    NothingHeard {
        /// The provider that transcribed it.
        stt: ProviderId,
    },
}

impl From<TranscribeError> for DictationError {
    fn from(error: TranscribeError) -> Self {
        match error {
            TranscribeError::NoProvider { excluded } => Self::NoSttProvider { excluded },
            TranscribeError::Failed { failures } => Self::SttFailed { failures },
            TranscribeError::Interrupted(interrupted) => Self::Interrupted(interrupted),
        }
    }
}

/// Dictation could not produce any text.
#[derive(Debug, thiserror::Error)]
pub enum DictationError {
    /// Policy or language support excluded every speech-to-text provider.
    #[error("no speech-to-text provider may handle this request")]
    NoSttProvider {
        /// Why each provider was excluded.
        excluded: Vec<Exclusion>,
    },
    /// Every eligible speech-to-text provider failed.
    #[error("speech-to-text failed")]
    SttFailed {
        /// Each provider tried, with its error.
        failures: Vec<(ProviderId, ProviderError)>,
    },
    /// The request was cancelled or ran out of time.
    #[error(transparent)]
    Interrupted(#[from] Interrupted),
}

/// Turns speech into clean text in the field the user was in.
///
/// Stages degrade instead of failing: if cleanup is unavailable, too slow or
/// rejected by the guard, the raw transcript is inserted. If the target is
/// gone, the text is held for the user.
pub struct DictationEngine {
    stt: Router<dyn SpeechToText>,
    rewriter: Option<Router<dyn Rewriter>>,
    desktop: Arc<dyn Desktop>,
    config: DictationConfig,
}

impl DictationEngine {
    /// An engine without cleanup, using the default config.
    #[must_use]
    pub fn new(stt: Router<dyn SpeechToText>, desktop: Arc<dyn Desktop>) -> Self {
        Self {
            stt,
            rewriter: None,
            desktop,
            config: DictationConfig::default(),
        }
    }

    /// Enables cleanup through `rewriter`.
    #[must_use]
    pub fn with_rewriter(mut self, rewriter: Router<dyn Rewriter>) -> Self {
        self.rewriter = Some(rewriter);
        self
    }

    /// Replaces the config.
    #[must_use]
    pub fn with_config(mut self, config: DictationConfig) -> Self {
        self.config = config;
        self
    }

    /// Runs one dictation.
    pub async fn run(
        &self,
        request: &DictationRequest,
        cx: &Ctx,
    ) -> Result<DictationOutcome, DictationError> {
        cx.check()?;
        let (stt, transcript) = self.transcribe(request, cx).await?;
        let raw = transcript.text.trim();
        if raw.is_empty() {
            return Ok(DictationOutcome::NothingHeard { stt });
        }

        cx.check()?;
        let (text, rewrite) = self
            .clean_up(raw, request, transcript.language.as_ref(), cx)
            .await;

        cx.check()?;
        let provenance = Provenance { stt, rewrite };
        Ok(self.insert(&request.anchor, text, provenance, cx).await)
    }

    async fn transcribe(
        &self,
        request: &DictationRequest,
        cx: &Ctx,
    ) -> Result<(ProviderId, Transcript), DictationError> {
        crate::stt::transcribe(&self.stt, &request.audio, &request.hints, cx)
            .await
            .map_err(Into::into)
    }

    /// Never fails: every problem falls back to the raw transcript.
    async fn clean_up(
        &self,
        raw: &str,
        request: &DictationRequest,
        language: Option<&LanguageTag>,
        cx: &Ctx,
    ) -> (String, RewriteStatus) {
        let Some(router) = &self.rewriter else {
            return (raw.to_owned(), RewriteStatus::Disabled);
        };
        let language = language.or(request.hints.language.as_ref());
        let Some(provider) = router
            .candidates(TRANSCRIPT_LABEL, language)
            .allowed
            .into_iter()
            .next()
        else {
            return (raw.to_owned(), RewriteStatus::NoEligibleProvider);
        };

        let id = provider.descriptor().id.clone();
        let budget = self
            .config
            .rewrite_budget
            .for_words(raw.split_whitespace().count());
        let budget = cx.remaining().map_or(budget, |left| left.min(budget));
        let rewrite_request = RewriteRequest {
            transcript: raw.to_owned(),
            app: Some(request.anchor.app.clone()),
            vocabulary: request.hints.vocabulary.clone(),
            language: language.cloned(),
        };

        let result = tokio::time::timeout(
            budget,
            provider.rewrite(&rewrite_request, &cx.child(budget)),
        )
        .await;
        match result {
            // Either clock may fire first: ours, or the adapter honoring the
            // child context's deadline. Both mean the budget ran out.
            Err(_) | Ok(Err(ProviderError::Interrupted(Interrupted::DeadlineExceeded))) => {
                tracing::warn!(provider = %id, ?budget, "cleanup timed out; using raw transcript");
                (raw.to_owned(), RewriteStatus::TimedOut(id))
            }
            Ok(Err(error)) => {
                tracing::warn!(provider = %id, %error, "cleanup failed; using raw transcript");
                (raw.to_owned(), RewriteStatus::Failed(id))
            }
            Ok(Ok(rewrite)) => {
                let cleaned = rewrite.text.trim();
                let check = RewriteCheck {
                    raw,
                    rewritten: cleaned,
                    vocabulary: &request.hints.vocabulary,
                };
                if self.config.guard.accepts(&check) {
                    (cleaned.to_owned(), RewriteStatus::Applied(id))
                } else {
                    tracing::warn!(provider = %id, "guard rejected cleanup; using raw transcript");
                    (
                        raw.to_owned(),
                        RewriteStatus::Rejected {
                            provider: id,
                            candidate: cleaned.to_owned(),
                        },
                    )
                }
            }
        }
    }

    /// Never fails: anything that prevents a safe insert holds the text instead.
    async fn insert(
        &self,
        anchor: &Anchor,
        text: String,
        provenance: Provenance,
        cx: &Ctx,
    ) -> DictationOutcome {
        let held = |text, reason| DictationOutcome::Held {
            text,
            reason,
            provenance: provenance.clone(),
        };
        let Some(element) = anchor.element.clone() else {
            return held(text, HoldReason::NoTargetElement);
        };

        let action = UiAction::InsertText {
            element,
            text: text.clone(),
        };
        match self.desktop.act(&anchor.window, &action, cx).await {
            Ok(report) => DictationOutcome::Inserted {
                text,
                verified: report.delivery == Delivery::Verified,
                provenance,
            },
            Err(DesktopError::WindowNotFound(_) | DesktopError::StaleElement(_)) => {
                held(text, HoldReason::TargetGone)
            }
            Err(DesktopError::NeedsForeground) => held(text, HoldReason::NeedsForeground),
            Err(error) => held(text, HoldReason::InsertFailed(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_grows_with_length_up_to_its_cap() {
        let budget = RewriteBudget::default();
        assert_eq!(budget.for_words(0), Duration::from_millis(400));
        assert_eq!(budget.for_words(10), Duration::from_millis(550));
        assert_eq!(budget.for_words(10_000), Duration::from_millis(1500));
        assert_eq!(budget.for_words(usize::MAX), Duration::from_millis(1500));
    }

    #[test]
    fn fixed_budget_ignores_length() {
        let budget = RewriteBudget::fixed(Duration::from_millis(400));
        assert_eq!(budget.for_words(0), budget.for_words(500));
    }
}
