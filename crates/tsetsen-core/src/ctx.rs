//! Per-request context handed to every port: identity, deadline, cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::ids::RequestId;

/// Cooperative cancellation shared by everything working on one request.
///
/// Clones share state: cancelling any clone cancels all of them.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    /// A token that is not cancelled yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the request. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the request has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Why a request stopped before it finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Interrupted {
    /// The user or the system cancelled the request.
    #[error("request was cancelled")]
    Cancelled,
    /// The request ran out of time.
    #[error("request deadline exceeded")]
    DeadlineExceeded,
}

/// Context for one request. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Ctx {
    request_id: RequestId,
    deadline: Option<Instant>,
    cancellation: Cancellation,
}

impl Ctx {
    /// A context with no deadline and a fresh cancellation token.
    #[must_use]
    pub fn new(request_id: RequestId) -> Self {
        Self {
            request_id,
            deadline: None,
            cancellation: Cancellation::new(),
        }
    }

    /// Uses `cancellation` instead of the context's own token.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: Cancellation) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Tightens the deadline to at most `timeout` from now. Never extends it.
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.child(timeout)
    }

    /// A context for a sub-step: same request and cancellation, with the
    /// deadline tightened to at most `budget` from now.
    #[must_use]
    pub fn child(&self, budget: Duration) -> Self {
        let candidate = Instant::now().checked_add(budget);
        let deadline = match (self.deadline, candidate) {
            (Some(current), Some(candidate)) => Some(current.min(candidate)),
            (current, candidate) => current.or(candidate),
        };
        Self {
            request_id: self.request_id,
            deadline,
            cancellation: self.cancellation.clone(),
        }
    }

    /// The request this context belongs to.
    #[must_use]
    pub fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// The deadline, if any.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// The shared cancellation token.
    #[must_use]
    pub fn cancellation(&self) -> &Cancellation {
        &self.cancellation
    }

    /// Time left before the deadline, if there is one. Zero once it has passed.
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Fails if the request was cancelled or its deadline has passed.
    pub fn check(&self) -> Result<(), Interrupted> {
        if self.cancellation.is_cancelled() {
            return Err(Interrupted::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Interrupted::DeadlineExceeded);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_never_extends_the_parent_deadline() {
        let parent = Ctx::new(RequestId::new(1)).with_timeout(Duration::from_millis(50));
        let child = parent.child(Duration::from_secs(60));
        assert_eq!(child.deadline(), parent.deadline());
    }

    #[test]
    fn child_tightens_the_deadline() {
        let parent = Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(60));
        let child = parent.child(Duration::from_millis(10));
        assert!(child.deadline() < parent.deadline());
    }

    #[test]
    fn cancellation_is_shared_with_children() {
        let parent = Ctx::new(RequestId::new(1));
        let child = parent.child(Duration::from_secs(1));
        parent.cancellation().cancel();
        assert_eq!(child.check(), Err(Interrupted::Cancelled));
    }

    #[test]
    fn passed_deadline_is_reported() {
        let cx = Ctx::new(RequestId::new(1)).with_timeout(Duration::ZERO);
        assert_eq!(cx.check(), Err(Interrupted::DeadlineExceeded));
        assert_eq!(cx.remaining(), Some(Duration::ZERO));
    }
}
