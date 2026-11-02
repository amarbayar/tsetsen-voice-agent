use std::time::Duration;

use async_trait::async_trait;

use crate::ports::AudioClip;
use crate::{AudioError, Ctx};

/// How much of a clip was heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Played {
    /// How much audio reached the speaker.
    pub duration: Duration,
    /// Whether the clip played to the end.
    pub completed: bool,
}

/// The speaker.
#[async_trait]
pub trait Playback: Send + Sync {
    /// Plays `clip`, returning when it ends. Cancelling `cx` stops it at once
    /// and returns how much was heard, so an interrupted reply can be recorded
    /// as only what the user actually heard.
    async fn play(&self, clip: &AudioClip, cx: &Ctx) -> Result<Played, AudioError>;
}
