//! Speech-to-text with routing and fallback, shared by every pipeline.

use tsetsen_core::ports::{AudioClip, SpeechToText, SttHints, Transcript};
use tsetsen_core::{Ctx, Interrupted, ProviderError, ProviderId, Sensitivity};
use tsetsen_router::{Exclusion, Router};

/// Raw audio is the most sensitive thing a pipeline handles.
pub(crate) const AUDIO_LABEL: Sensitivity = Sensitivity::Audio;
/// A transcript carries the words but not the voice, so it is labeled lower
/// than audio. This is the pipelines' one deliberate declassification.
pub(crate) const TRANSCRIPT_LABEL: Sensitivity = Sensitivity::Personal;

/// Speech could not be turned into text.
#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    /// Policy or language support excluded every provider.
    #[error("no speech-to-text provider may handle this request")]
    NoProvider {
        /// Why each provider was excluded.
        excluded: Vec<Exclusion>,
    },
    /// Every eligible provider failed.
    #[error("speech-to-text failed")]
    Failed {
        /// Each provider tried, with its error.
        failures: Vec<(ProviderId, ProviderError)>,
    },
    /// The request was cancelled or ran out of time.
    #[error(transparent)]
    Interrupted(#[from] Interrupted),
}

/// Transcribes with the first eligible provider that succeeds.
pub(crate) async fn transcribe(
    router: &Router<dyn SpeechToText>,
    audio: &AudioClip,
    hints: &SttHints,
    cx: &Ctx,
) -> Result<(ProviderId, Transcript), TranscribeError> {
    let candidates = router.candidates(AUDIO_LABEL, hints.language.as_ref());
    if candidates.allowed.is_empty() {
        return Err(TranscribeError::NoProvider {
            excluded: candidates.excluded,
        });
    }
    let mut failures = Vec::new();
    for provider in candidates.allowed {
        let id = provider.descriptor().id.clone();
        match provider.transcribe(audio, hints, cx).await {
            Ok(transcript) => {
                tracing::debug!(provider = %id, request = %cx.request_id(), "transcribed");
                return Ok((id, transcript));
            }
            Err(ProviderError::Interrupted(interrupted)) => return Err(interrupted.into()),
            Err(error) => {
                tracing::warn!(provider = %id, %error, "speech-to-text failed");
                let fall_back = error.allows_fallback();
                failures.push((id, error));
                if !fall_back {
                    break;
                }
                cx.check()?;
            }
        }
    }
    Err(TranscribeError::Failed { failures })
}
