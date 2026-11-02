//! Sending things to people: a message, with any pictures or files found.
//!
//! Whatever the working set holds can be attached, so "send my son the
//! letter from the bank" works the same way as "send my son the pictures".
//! The user hears exactly what will be sent, and must say yes first.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tsetsen_core::Ctx;
use tsetsen_core::ports::{Contact, Mailbox, Mailer, OutgoingMail, ToolCall, ToolSpec};
use tsetsen_policy::Consequence;

use super::items::{Item, Kind};
use super::{
    Handles, MAX_ATTACHMENT_BYTES, Shared, args, explain, plural, spec, spoken_address, spoken_file,
};
use crate::tools::{Assessment, ToolOutput, Toolbox};

const SHARE: &str = "share";

/// Sending things to people.
pub(crate) struct ShareTools {
    shared: Arc<Shared>,
    mail: Arc<dyn Mailer>,
}

pub(crate) fn offer(shared: &Arc<Shared>) -> Option<Arc<dyn Toolbox>> {
    let mail = shared.caps.mail.clone()?;
    // Nobody to send to without a way to find people.
    if !shared.people.available() {
        return None;
    }
    Some(Arc::new(ShareTools {
        shared: Arc::clone(shared),
        mail,
    }))
}

#[derive(Deserialize)]
struct ShareArgs {
    #[serde(alias = "recipient", alias = "recipients", alias = "people")]
    to: Handles,
    #[serde(default)]
    subject: String,
    #[serde(alias = "body", alias = "text")]
    message: String,
    #[serde(
        default,
        alias = "attach",
        alias = "attachments",
        alias = "pictures",
        alias = "files"
    )]
    items: Handles,
}

struct Recipient {
    contact: Contact,
    mailbox: Mailbox,
    /// Who they are to the user, if known or about to be learned.
    relation: Option<String>,
    /// The relation is new: remembered once the user approves and it sends.
    learn: bool,
}

/// One thing attached.
struct Attachment {
    kind: Kind,
    path: PathBuf,
    bytes: u64,
}

/// A checked message, ready to send.
struct Message {
    to: Vec<Recipient>,
    subject: String,
    body: String,
    attached: Vec<Attachment>,
    /// Things were found but none are attached: said out loud, because nobody
    /// notices what is not mentioned.
    left_out: Option<&'static str>,
}

/// What is attached, as it is said: "3 pictures and the file "Tax return"".
fn spoken_attachments(attached: &[Attachment]) -> String {
    let pictures = attached.iter().filter(|a| a.kind == Kind::Picture).count();
    let files: Vec<&Attachment> = attached.iter().filter(|a| a.kind == Kind::File).collect();
    let mut parts = Vec::new();
    match pictures {
        0 => {}
        1 => parts.push("the picture".to_owned()),
        n => parts.push(format!("{n} pictures")),
    }
    match files[..] {
        [] => {}
        [one] => {
            let name = one
                .path
                .file_name()
                .map_or_else(String::new, |n| spoken_file(&n.to_string_lossy()));
            parts.push(format!("the file \"{name}\""));
        }
        ref many => parts.push(plural(many.len(), "file", "files")),
    }
    parts.join(" and ")
}

impl ShareTools {
    /// Checks a call against what the conversation has found.
    fn plan(&self, call: &ToolCall) -> Result<Message, String> {
        let ShareArgs {
            to,
            subject,
            message,
            items,
        } = args(call)?;
        let set = &self.shared.items;
        let mut recipients = Vec::new();
        for handle in to.into_vec() {
            let Some(Item::Person { contact, relation }) = set.get(&handle) else {
                return Err(format!(
                    "`{handle}` is not a person: use an id like c1 from find_person. Never type \
                     an address yourself."
                ));
            };
            let address = contact
                .emails
                .first()
                .ok_or_else(|| format!("{} has no email address.", contact.name))?
                .clone();
            recipients.push(Recipient {
                mailbox: Mailbox {
                    name: Some(contact.name.clone()),
                    address,
                },
                contact,
                learn: false,
                relation,
            });
        }
        if recipients.is_empty() {
            return Err("Say who to send it to, with an id like c1 from find_person.".into());
        }
        let mut attached: Vec<Attachment> = Vec::new();
        for handle in items.into_vec() {
            let item = set.get(&handle).ok_or_else(|| set.unknown(&handle))?;
            let (path, bytes) = item
                .file()
                .ok_or_else(|| format!("`{handle}` is a person, not something to attach."))?;
            if !attached.iter().any(|a| a.path == path) {
                attached.push(Attachment {
                    kind: item.kind(),
                    path,
                    bytes,
                });
            }
        }
        let bytes: u64 = attached.iter().map(|a| a.bytes).sum();
        if bytes > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "Those {} are too large for one email. Ask the user to choose fewer, or send \
                 them in two emails.",
                plural(attached.len(), "thing", "things")
            ));
        }
        let body = message.trim().to_owned();
        if body.is_empty() {
            return Err("Write a short message, in the user's words.".into());
        }
        let subject = match subject.trim() {
            "" if attached.is_empty() => "A note".to_owned(),
            "" if attached.iter().all(|a| a.kind == Kind::Picture) => "Pictures".to_owned(),
            "" => "A file for you".to_owned(),
            s => s.to_owned(),
        };
        // "My son", then a name: the one person written to is taken to be
        // him, and the read-back says so, so the user's yes confirms it.
        if let ([only], Some(unknown)) =
            (&mut recipients[..], self.shared.people.unknown_relation())
            && only.relation.is_none()
        {
            only.relation = Some(unknown);
            only.learn = true;
        }
        let left_out = if !attached.is_empty() {
            None
        } else if set.has(Kind::File) {
            Some("nothing attached")
        } else if set.has(Kind::Picture) {
            Some("no pictures")
        } else {
            None
        };
        Ok(Message {
            to: recipients,
            subject,
            body,
            attached,
            left_out,
        })
    }

    async fn send(&self, call: &ToolCall, cx: &Ctx) -> ToolOutput {
        let message = match self.plan(call) {
            Ok(message) => message,
            Err(problem) => return ToolOutput::failed(problem),
        };
        let names: Vec<String> = message.to.iter().map(|r| r.contact.name.clone()).collect();
        let what = match (spoken_attachments(&message.attached), message.left_out) {
            (s, _) if !s.is_empty() => format!("with {s}"),
            (_, Some(left_out)) => format!("with {left_out}"),
            _ => "with nothing attached".to_owned(),
        };
        let learned: Vec<(String, Contact)> = message
            .to
            .iter()
            .filter(|r| r.learn)
            .filter_map(|r| Some((r.relation.clone()?, r.contact.clone())))
            .collect();
        let mail = OutgoingMail {
            to: message.to.into_iter().map(|r| r.mailbox).collect(),
            subject: message.subject,
            body: message.body,
            attachments: message.attached.into_iter().map(|a| a.path).collect(),
        };
        if let Err(error) = self.mail.send(&mail, cx).await {
            return ToolOutput::failed(explain(&error, "Email"));
        }
        super::show::clear(&self.shared, cx).await;
        let mut out = format!("Sent to {}, {what}.", names.join(" and "));
        for (relation, contact) in learned {
            let name = contact.name.clone();
            if self
                .shared
                .people
                .learn(&relation, contact, cx)
                .await
                .is_ok()
            {
                let _ = write!(out, " Remembered that {name} is the user's {relation}.");
            }
        }
        ToolOutput::ok(out)
    }
}

/// The read-back: exactly what will be sent, said the way a person says it.
fn summary(message: &Message) -> String {
    let to: Vec<String> = message
        .to
        .iter()
        .map(|r| {
            let who = match &r.relation {
                Some(relation) => format!("your {relation}, {}", r.contact.name),
                None => r.contact.name.clone(),
            };
            format!("{who}, at {},", spoken_address(&r.mailbox.address))
        })
        .collect();
    let attached = match (spoken_attachments(&message.attached), message.left_out) {
        (s, _) if !s.is_empty() => format!(" {s},"),
        (_, Some(left_out)) => format!(" with {left_out},"),
        _ => String::new(),
    };
    format!(
        "email {}{attached} with the subject \"{}\" and the message: \"{}\"",
        to.join(" and "),
        message.subject,
        message.body
    )
}

#[async_trait]
impl Toolbox for ShareTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![spec(
            SHARE,
            "Send a message to people from the user's own email, with anything found attached: \
             pictures, files. The user hears exactly what will be sent and must say yes first. \
             Use this for all email; never operate a mail app or website.",
            r#"{"type":"object","properties":{"to":{"type":"array","items":{"type":"string"},"description":"Person ids, e.g. [\"c1\"]"},"subject":{"type":"string"},"message":{"type":"string","description":"Short and warm, in the user's own voice"},"items":{"type":"array","items":{"type":"string"},"description":"What to attach: ids like p1 or f2"}},"required":["to","message"]}"#,
        )]
    }

    async fn assess(&self, call: &ToolCall, _cx: &Ctx) -> Result<Assessment, String> {
        let message = self.plan(call)?;
        Ok(Assessment {
            summary: summary(&message),
            consequence: Consequence::Commit,
            progress: Some("Sending your email".into()),
        })
    }

    async fn run(&self, call: &ToolCall, cx: &Ctx) -> ToolOutput {
        match call.name.as_str() {
            SHARE => self.send(call, cx).await,
            other => ToolOutput::failed(format!("There is no tool named `{other}`.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(kind: Kind, path: &str) -> Attachment {
        Attachment {
            kind,
            path: path.into(),
            bytes: 1,
        }
    }

    #[test]
    fn says_what_is_attached() {
        let pictures = [
            attachment(Kind::Picture, "/a.jpg"),
            attachment(Kind::Picture, "/b.jpg"),
        ];
        assert_eq!(spoken_attachments(&pictures), "2 pictures");
        let mixed = [
            attachment(Kind::Picture, "/a.jpg"),
            attachment(Kind::File, "/docs/Tax_return-2025.pdf"),
        ];
        assert_eq!(
            spoken_attachments(&mixed),
            "the picture and the file \"Tax return 2025\""
        );
        assert_eq!(spoken_attachments(&[]), "");
    }
}
