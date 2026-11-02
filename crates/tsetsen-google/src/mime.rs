//! RFC 5322 messages with attachments, for Gmail to send as they are.
//!
//! Every user-supplied header value is MIME-encoded, so a name or subject
//! can never inject a header.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tsetsen_core::ports::Mailbox;

/// A file to attach.
pub(crate) struct Attachment {
    pub(crate) filename: String,
    pub(crate) content: Vec<u8>,
}

fn encoded_word(text: &str) -> String {
    format!("=?UTF-8?B?{}?=", STANDARD.encode(text))
}

/// Whether `address` is safe to put in a header as it is.
pub(crate) fn valid_address(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && address.chars().all(|c| {
            c.is_ascii_graphic() && !matches!(c, '<' | '>' | ',' | ';' | '"' | '(' | ')' | '\\')
        })
}

fn mailbox(m: &Mailbox) -> String {
    match &m.name {
        Some(name) if !name.trim().is_empty() => {
            format!("{} <{}>", encoded_word(name.trim()), m.address)
        }
        _ => format!("<{}>", m.address),
    }
}

fn content_type(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    match lower.rsplit('.').next() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("heic") => "image/heic",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Base64, wrapped at 76 columns as MIME requires.
fn wrapped_base64(bytes: &[u8]) -> String {
    let encoded = STANDARD.encode(bytes);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2 + 2);
    for line in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push_str("\r\n");
    }
    out
}

/// A filename safe inside a quoted header parameter.
fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .collect();
    if cleaned.trim().is_empty() {
        "attachment".into()
    } else {
        cleaned
    }
}

/// The whole message. `boundary` must not occur in any part; random is safe.
pub(crate) fn message(
    to: &[Mailbox],
    subject: &str,
    body: &str,
    attachments: &[Attachment],
    boundary: &str,
) -> String {
    let mut out = String::new();
    let recipients: Vec<String> = to.iter().map(mailbox).collect();
    let _ = write!(
        out,
        "To: {}\r\nSubject: {}\r\nMIME-Version: 1.0\r\n",
        recipients.join(", "),
        encoded_word(subject)
    );
    let text = wrapped_base64(body.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
    if attachments.is_empty() {
        let _ = write!(
            out,
            "Content-Type: text/plain; charset=\"UTF-8\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{text}"
        );
        return out;
    }
    let _ = write!(
        out,
        "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n\
         --{boundary}\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{text}"
    );
    for attachment in attachments {
        let name = safe_filename(&attachment.filename);
        let _ = write!(
            out,
            "--{boundary}\r\nContent-Type: {}; name=\"{name}\"\r\n\
             Content-Disposition: attachment; filename=\"{name}\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{}",
            content_type(&name),
            wrapped_base64(&attachment.content)
        );
    }
    let _ = write!(out, "--{boundary}--\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to(name: &str, address: &str) -> Mailbox {
        Mailbox {
            name: Some(name.into()),
            address: address.into(),
        }
    }

    #[test]
    fn headers_cannot_be_injected() {
        let message = message(
            &[to("David\r\nBcc: evil@example.com", "david@example.com")],
            "Hi\r\nBcc: evil@example.com",
            "Hello",
            &[],
            "b",
        );
        let headers = message.split("\r\n\r\n").next().unwrap();
        assert!(!headers.contains("Bcc"), "{headers}");
        assert!(headers.starts_with("To: =?UTF-8?B?"));
    }

    #[test]
    fn builds_a_multipart_message_with_pictures() {
        let message = message(
            &[to("Dávid", "david@example.com")],
            "Our trip ☀️",
            "Here they are.\nLove, Mom",
            &[Attachment {
                filename: "picture-01.jpg".into(),
                content: vec![0xFF, 0xD8, 0xFF, 0xE0],
            }],
            "tsetsen-1",
        );
        assert!(message.contains("Content-Type: multipart/mixed; boundary=\"tsetsen-1\""));
        assert!(message.contains("Content-Type: image/jpeg; name=\"picture-01.jpg\""));
        assert!(message.contains(&STANDARD.encode("Here they are.\r\nLove, Mom")));
        assert!(message.ends_with("--tsetsen-1--\r\n"));
        assert!(
            message.lines().all(|l| l.len() <= 998),
            "RFC 5322 line limit"
        );
    }

    #[test]
    fn only_plain_addresses_are_accepted() {
        assert!(valid_address("david.smith+trip@example.co.uk"));
        for bad in [
            "david",
            "@example.com",
            "david@example",
            "a@b.com>, evil@x.com",
            "david@example.com\r\nBcc: x@y.z",
            "da vid@example.com",
        ] {
            assert!(!valid_address(bad), "{bad:?}");
        }
    }
}
