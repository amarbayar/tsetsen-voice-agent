//! Running agent-desktop commands and reading their JSON envelopes.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tsetsen_core::{Ctx, DesktopError, ElementRef, Interrupted, WindowId};

/// How often cancellation is checked while a command runs.
const POLL: Duration = Duration::from_millis(10);

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    data: Option<Value>,
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
pub(crate) struct ErrorBody {
    pub(crate) code: String,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) details: Value,
    pub(crate) disposition: Option<Disposition>,
}

#[derive(Deserialize)]
pub(crate) struct Disposition {
    #[serde(default)]
    pub(crate) delivery: String,
}

/// What a failed command was aimed at, for precise errors.
#[derive(Clone, Copy, Default)]
pub(crate) struct Target<'a> {
    pub(crate) app: Option<&'a str>,
    pub(crate) window: Option<&'a WindowId>,
    pub(crate) element: Option<&'a ElementRef>,
}

/// Runs `binary` with `args` and returns the envelope's `data`.
///
/// Arguments go straight to the process, never through a shell. Neither the
/// arguments nor the output are logged: they hold typed text and screen
/// content.
pub(crate) async fn run(
    binary: &Path,
    args: &[&str],
    timeout: Duration,
    target: Target<'_>,
    cx: &Ctx,
) -> Result<Value, DesktopError> {
    cx.check()?;
    let (timeout, by_deadline) = match cx.remaining() {
        Some(left) if left < timeout => (left, true),
        _ => (timeout, false),
    };
    let child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // An interrupted command must not keep acting on the desktop.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| DesktopError::Platform(format!("cannot start agent-desktop: {e}")))?;

    let output = child.wait_with_output();
    tokio::pin!(output);
    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    let mut tick = tokio::time::interval(POLL);
    let output = loop {
        tokio::select! {
            biased;
            output = &mut output => break output,
            () = &mut sleep => {
                return Err(if by_deadline {
                    Interrupted::DeadlineExceeded.into()
                } else {
                    DesktopError::ActionFailed {
                        message: "agent-desktop timed out".into(),
                        maybe_delivered: true,
                    }
                });
            }
            _ = tick.tick() => cx.check()?,
        }
    }
    .map_err(|e| DesktopError::Platform(format!("agent-desktop failed: {e}")))?;

    let envelope: Envelope = serde_json::from_slice(&output.stdout).map_err(|e| {
        DesktopError::Platform(format!(
            "agent-desktop printed no valid result (exit {}): {e}",
            output.status
        ))
    })?;
    match (envelope.ok, envelope.data, envelope.error) {
        (true, data, _) => Ok(data.unwrap_or(Value::Null)),
        (false, _, Some(error)) => Err(to_desktop_error(&error, target)),
        (false, _, None) => Err(DesktopError::Platform(
            "agent-desktop failed without saying why".into(),
        )),
    }
}

/// Maps agent-desktop's error codes onto the port's errors. Anything that
/// may have been delivered says so, so nobody retries a send blindly.
pub(crate) fn to_desktop_error(error: &ErrorBody, target: Target<'_>) -> DesktopError {
    let window = || target.window.cloned().unwrap_or_else(|| WindowId::new("?"));
    let delivered = error
        .disposition
        .as_ref()
        .is_none_or(|d| d.delivery != "not_delivered");
    match error.code.as_str() {
        "PERM_DENIED" => DesktopError::PermissionDenied(error.message.clone()),
        "APP_NOT_FOUND" => {
            DesktopError::AppNotFound(target.app.unwrap_or(&error.message).to_owned())
        }
        "WINDOW_NOT_FOUND" => DesktopError::WindowNotFound(window()),
        "ACTION_NOT_SUPPORTED"
            if error.details.get("kind").and_then(Value::as_str)
                == Some("window_without_accessibility_element") =>
        {
            DesktopError::NotOnScreen(window())
        }
        "STALE_REF" | "ELEMENT_NOT_FOUND" | "AMBIGUOUS_TARGET" | "SNAPSHOT_NOT_FOUND"
            if target.element.is_some() =>
        {
            DesktopError::StaleElement(target.element.cloned().unwrap_or_else(|| "?".into()))
        }
        "PLATFORM_NOT_SUPPORTED" => DesktopError::Unsupported(error.message.clone()),
        _ => DesktopError::ActionFailed {
            message: error.message.clone(),
            maybe_delivered: delivered,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(json: &str) -> ErrorBody {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn maps_error_codes_and_keeps_delivery_honest() {
        let window = WindowId::new("w-1");
        let element = ElementRef::new("@s1:e2");
        let target = Target {
            app: Some("Notes"),
            window: Some(&window),
            element: Some(&element),
        };
        assert!(matches!(
            to_desktop_error(&error(r#"{"code":"APP_NOT_FOUND","message":"x"}"#), target),
            DesktopError::AppNotFound(app) if app == "Notes"
        ));
        assert!(matches!(
            to_desktop_error(&error(r#"{"code":"STALE_REF"}"#), target),
            DesktopError::StaleElement(e) if e == element
        ));
        assert!(matches!(
            to_desktop_error(
                &error(
                    r#"{"code":"ACTION_NOT_SUPPORTED","details":{"kind":"window_without_accessibility_element"}}"#
                ),
                target
            ),
            DesktopError::NotOnScreen(w) if w == window
        ));
        assert!(matches!(
            to_desktop_error(
                &error(
                    r#"{"code":"ACTION_FAILED","message":"no","disposition":{"delivery":"not_delivered","retry":"safe"}}"#
                ),
                target
            ),
            DesktopError::ActionFailed {
                maybe_delivered: false,
                ..
            }
        ));
        // Unknown delivery: assume it may have happened.
        assert!(matches!(
            to_desktop_error(
                &error(r#"{"code":"APP_UNRESPONSIVE","disposition":{"delivery":"unknown"}}"#),
                target
            ),
            DesktopError::ActionFailed {
                maybe_delivered: true,
                ..
            }
        ));
        assert!(matches!(
            to_desktop_error(&error(r#"{"code":"TIMEOUT"}"#), target),
            DesktopError::ActionFailed {
                maybe_delivered: true,
                ..
            }
        ));
    }
}
