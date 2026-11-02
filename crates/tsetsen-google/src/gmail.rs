//! Sending mail through the Gmail API, as the signed-in user.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use tsetsen_core::ports::{Mailer, OutgoingMail, SentMail};
use tsetsen_core::{Ctx, ServiceError};

use crate::auth::Auth;
use crate::http::send;
use crate::mime::{Attachment, message, valid_address};

/// The most a message may hold, encoded; Gmail's limit is 35 MB.
const MAX_MESSAGE_BYTES: usize = 30 * 1024 * 1024;

/// Gmail, through its API.
#[derive(Debug)]
pub struct GmailMailer {
    auth: Arc<Auth>,
}

impl GmailMailer {
    /// Sends as the account `auth` signs in to.
    #[must_use]
    pub fn new(auth: Arc<Auth>) -> Self {
        Self { auth }
    }
}

#[derive(Deserialize)]
struct Sent {
    id: String,
}

async fn read_attachments(mail: &OutgoingMail) -> Result<Vec<Attachment>, ServiceError> {
    let mut attachments = Vec::with_capacity(mail.attachments.len());
    for path in &mail.attachments {
        let content = tokio::fs::read(path).await.map_err(|e| {
            ServiceError::Rejected(format!(
                "cannot read the attachment {}: {e}",
                path.display()
            ))
        })?;
        let filename = path
            .file_name()
            .map_or_else(|| "attachment".into(), |n| n.to_string_lossy().into_owned());
        attachments.push(Attachment { filename, content });
    }
    Ok(attachments)
}

fn boundary() -> String {
    let mut bytes = [0u8; 12];
    // A fixed fallback still works: base64 output can never contain "=_".
    let _ = getrandom::fill(&mut bytes);
    let hex = bytes.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    format!("=_tsetsen_{hex}")
}

/// Checks `mail` and renders it as the message Gmail is given.
pub(crate) async fn render(mail: &OutgoingMail) -> Result<String, ServiceError> {
    if mail.to.is_empty() {
        return Err(ServiceError::Rejected("no recipients".into()));
    }
    if let Some(bad) = mail.to.iter().find(|m| !valid_address(&m.address)) {
        return Err(ServiceError::Rejected(format!(
            "not an email address: {}",
            bad.address
        )));
    }
    let attachments = read_attachments(mail).await?;
    let raw = message(
        &mail.to,
        &mail.subject,
        &mail.body,
        &attachments,
        &boundary(),
    );
    if raw.len() > MAX_MESSAGE_BYTES {
        return Err(ServiceError::Rejected(
            "the message is too large for Gmail".into(),
        ));
    }
    Ok(raw)
}

#[async_trait]
impl Mailer for GmailMailer {
    async fn send(&self, mail: &OutgoingMail, cx: &Ctx) -> Result<SentMail, ServiceError> {
        let raw = render(mail).await?;
        let url = format!(
            "{}/upload/gmail/v1/users/me/messages/send?uploadType=media",
            self.auth.endpoints().gmail
        );
        // A rejected token means nothing was sent: refresh and retry once.
        for attempt in 0..2 {
            let token = self.auth.token(cx).await?;
            let request = self
                .auth
                .http()
                .post(&url)
                .bearer_auth(token.expose())
                .header("Content-Type", "message/rfc822")
                .body(raw.clone())
                .send();
            let response = match send(cx, request).await {
                Ok(Ok(response)) => response,
                // Never connected: nothing was sent.
                Ok(Err(e)) if e.is_connect() => {
                    return Err(ServiceError::Unavailable(format!(
                        "Gmail is unreachable: {e}"
                    )));
                }
                Ok(Err(e)) => {
                    return Err(ServiceError::Uncertain(format!(
                        "the connection to Gmail failed: {e}"
                    )));
                }
                Err(_) => {
                    return Err(ServiceError::Uncertain(
                        "interrupted while the message was being sent".into(),
                    ));
                }
            };
            let status = response.status();
            if status.is_success() {
                let sent: Sent = response.json().await.map_err(|_| {
                    ServiceError::Uncertain(
                        "Gmail accepted the message but sent back nonsense".into(),
                    )
                })?;
                return Ok(SentMail { id: sent.id });
            }
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.auth.invalidate().await;
                continue;
            }
            return Err(match status {
                StatusCode::UNAUTHORIZED => ServiceError::NotSetUp(
                    "Gmail no longer accepts the sign-in; sign in again".into(),
                ),
                StatusCode::FORBIDDEN => ServiceError::PermissionDenied(
                    "Gmail refused: sending is not allowed for this sign-in".into(),
                ),
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    ServiceError::Unavailable(format!("Gmail is busy ({status})"))
                }
                s if s.is_client_error() => {
                    ServiceError::Rejected(format!("Gmail refused the message ({s})"))
                }
                s => ServiceError::Uncertain(format!("Gmail failed ({s})")),
            });
        }
        Err(ServiceError::NotSetUp(
            "Gmail no longer accepts the sign-in".into(),
        ))
    }
}
