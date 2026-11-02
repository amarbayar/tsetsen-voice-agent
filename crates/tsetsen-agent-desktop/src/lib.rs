//! Desktop control for Tsetsen through [agent-desktop], which reads and
//! operates apps through the OS accessibility tree on macOS, Windows and
//! Linux.
//!
//! agent-desktop runs as a separate process per command. That keeps its
//! native code (and any crash) out of the voice loop, needs no `unsafe` here,
//! and makes upgrading a binary swap. On macOS it keeps a private helper
//! alive between calls, so a command costs tens of milliseconds.
//!
//! Reads and clicks are headless: they go through accessibility actions and
//! never move the pointer or steal focus. Key presses are real keystrokes, so
//! they bring their window to the front first.
//!
//! [agent-desktop]: https://github.com/vercel-labs/agent-desktop

mod cli;
mod convert;

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tsetsen_core::ports::{ActionReport, Desktop, TextEntry, UiAction, WindowInfo, WindowSnapshot};
use tsetsen_core::{Ctx, DesktopError, RequestId, WindowId};

use crate::cli::{Target, run};

/// The agent-desktop major version this adapter speaks.
pub const SUPPORTED_MAJOR: &str = "1";

/// How to run agent-desktop.
#[derive(Clone, Debug)]
pub struct AgentDesktopConfig {
    /// The `agent-desktop` executable. Its helper must sit beside it.
    pub binary: PathBuf,
    /// The longest any one command may take.
    pub timeout: Duration,
    /// The longest an app may take to launch and show a window.
    pub launch_timeout: Duration,
}

impl AgentDesktopConfig {
    /// Defaults for `binary`: 15 s per command, 30 s per launch.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            timeout: Duration::from_secs(15),
            launch_timeout: Duration::from_secs(30),
        }
    }
}

/// agent-desktop could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// No executable at the configured path.
    #[error("agent-desktop not found at {0}")]
    NotFound(PathBuf),
    /// A version this adapter does not speak.
    #[error("agent-desktop {found} is not supported; version {SUPPORTED_MAJOR}.x is required")]
    Incompatible {
        /// The version it reported.
        found: String,
    },
    /// The OS has not granted accessibility access.
    #[error(
        "accessibility access is not granted; run `{binary} permissions --guide` and allow the app \
         you run tsetsen from"
    )]
    NoAccessibility {
        /// The binary, for the instructions.
        binary: String,
    },
    /// It failed to answer.
    #[error(transparent)]
    Desktop(#[from] DesktopError),
}

/// The [`Desktop`] port over agent-desktop.
#[derive(Debug)]
pub struct AgentDesktop {
    config: AgentDesktopConfig,
}

impl AgentDesktop {
    /// Checks the binary's version and accessibility permission.
    pub async fn connect(config: AgentDesktopConfig) -> Result<Self, ConnectError> {
        if !config.binary.is_file() {
            return Err(ConnectError::NotFound(config.binary));
        }
        let desktop = Self { config };
        let cx = Ctx::new(RequestId::new(0)).with_timeout(Duration::from_secs(20));
        let version = desktop.run(&["version"], Target::default(), &cx).await?;
        let found = version
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        if found.split('.').next() != Some(SUPPORTED_MAJOR) {
            return Err(ConnectError::Incompatible { found });
        }
        let permissions = desktop
            .run(&["permissions"], Target::default(), &cx)
            .await?;
        if permissions
            .pointer("/accessibility/state")
            .and_then(Value::as_str)
            != Some("granted")
        {
            return Err(ConnectError::NoAccessibility {
                binary: desktop.config.binary.display().to_string(),
            });
        }
        tracing::debug!(version = %found, "agent-desktop ready");
        Ok(desktop)
    }

    async fn run(
        &self,
        args: &[&str],
        target: Target<'_>,
        cx: &Ctx,
    ) -> Result<Value, DesktopError> {
        run(&self.config.binary, args, self.config.timeout, target, cx).await
    }

    async fn window(&self, id: &WindowId, cx: &Ctx) -> Result<WindowInfo, DesktopError> {
        self.windows(cx)
            .await?
            .into_iter()
            .find(|w| &w.id == id)
            .ok_or_else(|| DesktopError::WindowNotFound(id.clone()))
    }
}

#[async_trait]
impl Desktop for AgentDesktop {
    /// `type` runs headless, through the accessibility API: it never sends
    /// key events, so a newline cannot press Enter.
    fn text_entry(&self) -> TextEntry {
        TextEntry::Inserted
    }

    async fn installed_apps(&self, _cx: &Ctx) -> Result<Vec<String>, DesktopError> {
        // A handful of directory reads; off the async threads all the same.
        tokio::task::spawn_blocking(installed_apps)
            .await
            .map_err(|e| DesktopError::Platform(format!("listing apps failed: {e}")))?
    }

    async fn open_app(&self, app: &str, cx: &Ctx) -> Result<WindowInfo, DesktopError> {
        let timeout_ms = self.config.launch_timeout.as_millis().to_string();
        let target = Target {
            app: Some(app),
            ..Target::default()
        };
        let data = run(
            &self.config.binary,
            &["launch", "--activate", "--timeout", &timeout_ms, "--", app],
            self.config.launch_timeout + Duration::from_secs(2),
            target,
            cx,
        )
        .await?;
        if let Some(window) = data.get("window") {
            return convert::window(window);
        }
        let name = data.get("app").and_then(Value::as_str).unwrap_or(app);
        self.windows(cx)
            .await?
            .into_iter()
            .find(|w| w.app == name)
            .ok_or_else(|| DesktopError::ActionFailed {
                message: format!("{name} started but showed no window"),
                maybe_delivered: true,
            })
    }

    async fn windows(&self, cx: &Ctx) -> Result<Vec<WindowInfo>, DesktopError> {
        let data = self.run(&["list-windows"], Target::default(), cx).await?;
        convert::windows(&data)
    }

    async fn snapshot(&self, window: &WindowId, cx: &Ctx) -> Result<WindowSnapshot, DesktopError> {
        let info = self.window(window, cx).await?;
        let target = Target {
            window: Some(window),
            ..Target::default()
        };
        let data = self
            .run(
                &["snapshot", "--compact", "--window-id", window.as_str()],
                target,
                cx,
            )
            .await?;
        convert::snapshot(&data, &info)
    }

    async fn act(
        &self,
        window: &WindowId,
        action: &UiAction,
        cx: &Ctx,
    ) -> Result<ActionReport, DesktopError> {
        let data = match action {
            UiAction::InsertText { element, text } => {
                let target = Target {
                    window: Some(window),
                    element: Some(element),
                    ..Target::default()
                };
                // `--` so text like "--force" is typed, never parsed.
                self.run(&["type", "--", element.as_str(), text], target, cx)
                    .await?
            }
            UiAction::Click { element } => {
                let target = Target {
                    window: Some(window),
                    element: Some(element),
                    ..Target::default()
                };
                self.run(&["click", "--", element.as_str()], target, cx)
                    .await?
            }
            UiAction::PressKeys { combo } => {
                let target = Target {
                    window: Some(window),
                    ..Target::default()
                };
                // Keystrokes go to the focused window, so focus it first and
                // name it again, so they cannot land anywhere else.
                self.run(
                    &["focus-window", "--headed", "--window-id", window.as_str()],
                    target,
                    cx,
                )
                .await?;
                self.run(
                    &[
                        "press",
                        "--headed",
                        "--window-id",
                        window.as_str(),
                        "--",
                        combo,
                    ],
                    target,
                    cx,
                )
                .await?
            }
            other => {
                return Err(DesktopError::Unsupported(format!("{other:?}")));
            }
        };
        Ok(ActionReport {
            delivery: convert::delivery(&data),
        })
    }
}

/// Display names of the apps in the standard locations, one folder deep.
fn installed_apps() -> Result<Vec<String>, DesktopError> {
    if !cfg!(target_os = "macos") {
        return Err(DesktopError::Unsupported(
            "listing installed apps is macOS-only for now".into(),
        ));
    }
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/Applications/Utilities"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(Path::new(&home).join("Applications"));
    }
    let mut apps = Vec::new();
    for root in roots {
        collect_apps(&root, 1, &mut apps);
    }
    apps.sort_unstable();
    apps.dedup();
    Ok(apps)
}

fn collect_apps(dir: &Path, depth: usize, apps: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.extension().is_some_and(|e| e == "app") {
            if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                apps.push(name.to_owned());
            }
        } else if depth > 0 {
            collect_apps(&path, depth - 1, apps); // e.g. /Applications/Adobe Photoshop/…
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn finds_the_system_apps() {
        let apps = installed_apps().unwrap();
        for app in ["Calculator", "TextEdit", "Notes"] {
            assert!(apps.iter().any(|a| a == app), "{app} missing from {apps:?}");
        }
        assert!(
            apps.windows(2).all(|w| w[0] < w[1]),
            "sorted, no duplicates"
        );
    }
}
