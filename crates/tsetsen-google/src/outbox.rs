//! A mailer that writes each message to a folder instead of sending it:
//! the exact bytes Gmail would have been given. For trying things safely.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tsetsen_core::ports::{Mailer, OutgoingMail, SentMail};
use tsetsen_core::{Ctx, ServiceError};

use crate::gmail::render;

/// Writes `.eml` files into a folder.
#[derive(Debug)]
pub struct OutboxMailer {
    folder: PathBuf,
    count: AtomicU64,
}

impl OutboxMailer {
    /// Writes into `folder`, creating it if needed.
    #[must_use]
    pub fn new(folder: impl Into<PathBuf>) -> Self {
        Self {
            folder: folder.into(),
            count: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl Mailer for OutboxMailer {
    async fn send(&self, mail: &OutgoingMail, cx: &Ctx) -> Result<SentMail, ServiceError> {
        cx.check()?;
        let raw = render(mail).await?;
        let unavailable = |e: std::io::Error| {
            ServiceError::Unavailable(format!("cannot write to the outbox: {e}"))
        };
        tokio::fs::create_dir_all(&self.folder)
            .await
            .map_err(unavailable)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        let path = self.folder.join(format!("{stamp}-{n}.eml"));
        tokio::fs::write(&path, raw).await.map_err(unavailable)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .await
                .map_err(unavailable)?;
        }
        Ok(SentMail {
            id: path.display().to_string(),
        })
    }
}
