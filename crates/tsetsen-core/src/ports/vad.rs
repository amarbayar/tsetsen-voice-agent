use crate::ProviderError;

/// Voice activity detection: how likely a short frame of audio is speech.
///
/// Stateful and synchronous: it runs on every captured frame, a few
/// milliseconds apart, so it is not routed like other model providers.
pub trait VoiceActivity: Send {
    /// The sample rate frames must have.
    fn sample_rate_hz(&self) -> u32;

    /// The number of samples per frame.
    fn frame_len(&self) -> usize;

    /// The probability, in `[0, 1]`, that `frame` is speech. `frame` must be
    /// exactly [`Self::frame_len`] samples.
    fn speech_probability(&mut self, frame: &[f32]) -> Result<f32, ProviderError>;

    /// Forgets the audio seen so far, e.g. between conversations.
    fn reset(&mut self);
}
