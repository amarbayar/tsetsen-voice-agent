//! Endpoints and request plumbing shared by every Google service.

use std::future::Future;
use std::time::Duration;

use tsetsen_core::{Ctx, Interrupted, ServiceError};

const POLL: Duration = Duration::from_millis(10);

/// Where Google's services are. Overridable for tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// The consent page.
    pub auth: String,
    /// Token exchange and refresh.
    pub token: String,
    /// The Gmail API root.
    pub gmail: String,
    /// The People API root.
    pub people: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            gmail: "https://gmail.googleapis.com".into(),
            people: "https://people.googleapis.com".into(),
        }
    }
}

impl Endpoints {
    /// Every service at `root`, for a test server.
    #[must_use]
    pub fn at(root: &str) -> Self {
        let root = root.trim_end_matches('/');
        Self {
            auth: format!("{root}/auth"),
            token: format!("{root}/token"),
            gmail: root.into(),
            people: root.into(),
        }
    }
}

/// Runs `request` until it finishes, `cx` is cancelled, or its deadline
/// passes.
pub(crate) async fn send<T>(cx: &Ctx, request: impl Future<Output = T>) -> Result<T, ServiceError> {
    cx.check()?;
    tokio::pin!(request);
    let deadline = async {
        match cx.remaining() {
            Some(left) => tokio::time::sleep(left).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            biased;
            done = &mut request => return Ok(done),
            () = &mut deadline => return Err(Interrupted::DeadlineExceeded.into()),
            _ = tick.tick() => cx.check()?,
        }
    }
}
