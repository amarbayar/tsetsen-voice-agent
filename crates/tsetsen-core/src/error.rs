//! Errors shared by all adapters of a port.

use crate::ctx::Interrupted;
use crate::ids::{ElementRef, WindowId};

/// A failure reported by a model provider adapter.
///
/// Variants describe what happened, not which vendor it happened at, so the
/// router can decide uniformly whether to fall back.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// The provider could not be reached or is not ready: worker down,
    /// network error, server error.
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    /// The provider asked us to slow down.
    #[error("provider rate limited the request")]
    RateLimited,
    /// This provider cannot handle the input (language, length, format),
    /// but another one might.
    #[error("unsupported input: {0}")]
    Unsupported(String),
    /// The request itself is invalid. Another provider will not do better.
    #[error("request rejected: {0}")]
    Rejected(String),
    /// The provider answered with something unusable.
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    /// The request was cancelled or ran out of time.
    #[error(transparent)]
    Interrupted(#[from] Interrupted),
}

impl ProviderError {
    /// Whether the next provider in the route should be tried.
    #[must_use]
    pub fn allows_fallback(&self) -> bool {
        !matches!(self, Self::Rejected(_) | Self::Interrupted(_))
    }
}

/// A failure reported by a desktop adapter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DesktopError {
    /// The target window is gone.
    #[error("window `{0}` no longer exists")]
    WindowNotFound(WindowId),
    /// No app by that name is installed.
    #[error("no app named `{0}`")]
    AppNotFound(String),
    /// The element reference is out of date; take a new snapshot.
    #[error("element `{0}` is stale")]
    StaleElement(ElementRef),
    /// The window exists but is not on screen: on another desktop,
    /// minimized, or the screen is locked.
    #[error("window `{0}` is not on screen")]
    NotOnScreen(WindowId),
    /// The action needs the window in the foreground, and it is not.
    #[error("action needs the window in the foreground")]
    NeedsForeground,
    /// The OS denied access, e.g. a missing Accessibility grant.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// This platform cannot perform the action.
    #[error("unsupported on this platform: {0}")]
    Unsupported(String),
    /// The request was cancelled or ran out of time.
    #[error(transparent)]
    Interrupted(#[from] Interrupted),
    /// The action failed. If `maybe_delivered`, it may still have taken
    /// effect: check before repeating it.
    #[error("action failed: {message}")]
    ActionFailed {
        /// What went wrong, safe to show the user.
        message: String,
        /// Whether the action may have taken effect anyway.
        maybe_delivered: bool,
    },
    /// Any other platform failure.
    #[error("platform error: {0}")]
    Platform(String),
}

/// A failure reported by a personal-data service: contacts, photos, mail.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServiceError {
    /// The service needs setting up first: signing in, installing a helper.
    #[error("not set up: {0}")]
    NotSetUp(String),
    /// The user or the OS has not granted access.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The service could not be reached; trying again later may work.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// The request itself is invalid; repeating it will not help.
    #[error("rejected: {0}")]
    Rejected(String),
    /// It failed in a way that may still have taken effect, e.g. a timeout
    /// after a message was handed over. Check before repeating it.
    #[error("may or may not have happened: {0}")]
    Uncertain(String),
    /// The request was cancelled or ran out of time.
    #[error(transparent)]
    Interrupted(#[from] Interrupted),
}

/// A failure reported by an audio device adapter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AudioError {
    /// No usable input or output device.
    #[error("no audio device: {0}")]
    NoDevice(String),
    /// The OS denied access, e.g. a missing microphone grant.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The device or stream failed.
    #[error("audio device error: {0}")]
    Device(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rejections_and_interruptions_stop_fallback() {
        assert!(ProviderError::Unavailable("down".into()).allows_fallback());
        assert!(ProviderError::RateLimited.allows_fallback());
        assert!(ProviderError::Unsupported("mn".into()).allows_fallback());
        assert!(ProviderError::InvalidResponse("bad json".into()).allows_fallback());
        assert!(!ProviderError::Rejected("empty".into()).allows_fallback());
        assert!(!ProviderError::Interrupted(Interrupted::Cancelled).allows_fallback());
    }
}
