use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;
use tsetsen_core::ports::Launcher;
use tsetsen_core::{Ctx, ServiceError};

/// Openers return as soon as the app is asked to open the file.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Opens files with the OS's own opener: `open` on macOS, `xdg-open` on
/// Linux, Explorer on Windows. The path is passed as one argument, never
/// through a shell.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemLauncher;

fn opener(path: &Path) -> Command {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer.exe"
    } else {
        "xdg-open"
    };
    let mut command = Command::new(program);
    if cfg!(target_os = "macos") {
        // Ends option parsing: a file named "-a" is still a file.
        command.arg("--");
    }
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

#[async_trait]
impl Launcher for SystemLauncher {
    async fn open(&self, path: &Path, cx: &Ctx) -> Result<(), ServiceError> {
        cx.check()?;
        if !path.is_absolute() || !path.exists() {
            return Err(ServiceError::Rejected(
                "that file is no longer there".into(),
            ));
        }
        let status = tokio::time::timeout(TIMEOUT, opener(path).status())
            .await
            .map_err(|_| ServiceError::Uncertain("the opener did not answer".into()))?
            .map_err(|e| ServiceError::NotSetUp(format!("cannot run the opener: {e}")))?;
        // Explorer's exit code means nothing; elsewhere failure means no app.
        if status.success() || cfg!(windows) {
            Ok(())
        } else {
            Err(ServiceError::Rejected(
                "no app on this computer opens that file".into(),
            ))
        }
    }
}
