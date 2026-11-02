use async_trait::async_trait;

use crate::ports::AudioClip;
use crate::{Ctx, Describe, LanguageTag, ProviderError, VoiceId};

/// Text to speak.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpeechRequest {
    /// A clause or sentence of plain text, without markup.
    pub text: String,
    /// Language of the text, if known.
    pub language: Option<LanguageTag>,
    /// Voice to use; the provider's default if `None`.
    pub voice: Option<VoiceId>,
}

/// The talker: renders text as speech.
///
/// Called once per clause, so short requests must be fast. The pipeline
/// overlaps rendering one clause with playing the previous one.
#[async_trait]
pub trait TextToSpeech: Describe + Send + Sync {
    /// Renders `request` at whatever sample rate the provider produces.
    async fn synthesize(
        &self,
        request: &SpeechRequest,
        cx: &Ctx,
    ) -> Result<AudioClip, ProviderError>;
}
