use std::path::PathBuf;

use async_trait::async_trait;

use crate::{Ctx, ServiceError};

/// An email address with an optional display name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mailbox {
    /// `David Smith`.
    pub name: Option<String>,
    /// `david@example.com`.
    pub address: String,
}

/// A message to send.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutgoingMail {
    /// Recipients.
    pub to: Vec<Mailbox>,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
    /// Files to attach, in order.
    pub attachments: Vec<PathBuf>,
}

/// A message that was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentMail {
    /// The provider's id for it.
    pub id: String,
}

/// Sends email as the user.
#[async_trait]
pub trait Mailer: Send + Sync {
    /// Sends `mail`. An error that may have sent it anyway is
    /// [`ServiceError::Uncertain`].
    async fn send(&self, mail: &OutgoingMail, cx: &Ctx) -> Result<SentMail, ServiceError>;
}
