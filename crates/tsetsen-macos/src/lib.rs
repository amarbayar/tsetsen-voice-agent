//! macOS services for Tsetsen: the photo library, a picture viewer, and
//! Spotlight file search.
//!
//! [`SpotlightFiles`] runs `mdfind`. The photo library and the viewer go through `tsetsen-photos`, a small Swift helper built with this
//! crate. It runs as a separate process per call, like agent-desktop: the
//! Objective-C runtime and its permission prompts stay out of the voice loop,
//! and this crate needs no `unsafe`.
//!
//! The library is only read. Exports are resized copies in a private folder
//! that is deleted when [`MacPhotos`] is dropped.

mod spotlight;

pub use spotlight::SpotlightFiles;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tsetsen_core::ports::{ExportedPhoto, GeoPoint, PhotoInfo, PhotoLibrary, PhotoQuery, Showcase};
use tsetsen_core::{Ctx, Interrupted, ServiceError};

/// The helper built with this crate. It exists only on macOS, and only if
/// `swiftc` was available at build time.
pub const BUILT_HELPER: &str = env!("TSETSEN_PHOTOS_HELPER");

const POLL: Duration = Duration::from_millis(10);
/// Searching even a large library takes well under this.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Exports may download originals from iCloud.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(120);
/// The viewer window should be up long before this.
const SHOW_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct Failure {
    error: FailureBody,
}

#[derive(Deserialize)]
struct FailureBody {
    code: String,
    message: String,
}

fn command(helper: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(helper);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

fn to_error(failure: FailureBody) -> ServiceError {
    match failure.code.as_str() {
        "PERMISSION_DENIED" => ServiceError::PermissionDenied(failure.message),
        "INVALID_ARGS" | "NOT_FOUND" => ServiceError::Rejected(failure.message),
        _ => ServiceError::Unavailable(failure.message),
    }
}

/// Runs the helper to completion and parses its JSON. Arguments go straight
/// to the process, never through a shell, and are never logged.
async fn run<T: DeserializeOwned>(
    helper: &Path,
    args: &[&str],
    timeout: Duration,
    cx: &Ctx,
) -> Result<T, ServiceError> {
    cx.check()?;
    if !helper.is_file() {
        return Err(ServiceError::NotSetUp(format!(
            "the photos helper is missing at {}",
            helper.display()
        )));
    }
    let (timeout, by_deadline) = match cx.remaining() {
        Some(left) if left < timeout => (left, true),
        _ => (timeout, false),
    };
    let child = command(helper, args)
        .spawn()
        .map_err(|e| ServiceError::Unavailable(format!("cannot start the photos helper: {e}")))?;
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
                    ServiceError::Unavailable("the photos helper timed out".into())
                });
            }
            _ = tick.tick() => cx.check()?,
        }
    }
    .map_err(|e| ServiceError::Unavailable(format!("the photos helper failed: {e}")))?;
    if !output.status.success() {
        return Err(match serde_json::from_slice::<Failure>(&output.stdout) {
            Ok(failure) => to_error(failure.error),
            Err(_) => {
                ServiceError::Unavailable(format!("the photos helper failed ({})", output.status))
            }
        });
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| ServiceError::Unavailable(format!("the photos helper said nonsense: {e}")))
}

#[derive(Deserialize)]
struct Found {
    id: String,
    taken: f64,
    #[serde(default)]
    favorite: bool,
    lat: Option<f64>,
    lon: Option<f64>,
}

impl Found {
    fn into_info(self) -> PhotoInfo {
        PhotoInfo {
            id: self.id,
            taken: UNIX_EPOCH + Duration::from_secs_f64(self.taken.max(0.0)),
            location: self
                .lat
                .zip(self.lon)
                .map(|(latitude, longitude)| GeoPoint {
                    latitude,
                    longitude,
                }),
            favorite: self.favorite,
        }
    }
}

#[derive(Deserialize)]
struct Exported {
    id: String,
    path: PathBuf,
    bytes: u64,
}

fn unix(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    format!("{secs:.3}")
}

/// The Photos library, through the helper.
#[derive(Debug)]
pub struct MacPhotos {
    helper: PathBuf,
    exports: PathBuf,
    batch: AtomicU64,
}

impl MacPhotos {
    /// Uses `helper`, exporting into a fresh private folder under the
    /// system's temporary directory.
    #[must_use]
    pub fn new(helper: impl Into<PathBuf>) -> Self {
        let exports = std::env::temp_dir().join(format!("tsetsen-pictures-{}", std::process::id()));
        Self {
            helper: helper.into(),
            exports,
            batch: AtomicU64::new(0),
        }
    }

    /// Whether the user has allowed access, without asking.
    pub async fn status(&self, cx: &Ctx) -> Result<String, ServiceError> {
        #[derive(Deserialize)]
        struct Status {
            photos: String,
        }
        let status: Status = run(&self.helper, &["status"], SEARCH_TIMEOUT, cx).await?;
        Ok(status.photos)
    }

    async fn fresh_folder(&self) -> Result<PathBuf, ServiceError> {
        let batch = self.batch.fetch_add(1, Ordering::Relaxed);
        let folder = self.exports.join(batch.to_string());
        let unavailable = |e: std::io::Error| {
            ServiceError::Unavailable(format!("cannot create {}: {e}", folder.display()))
        };
        tokio::fs::create_dir_all(&folder)
            .await
            .map_err(unavailable)?;
        #[cfg(unix)]
        {
            // The user's pictures: readable by the user only.
            use std::os::unix::fs::PermissionsExt;
            let private = std::fs::Permissions::from_mode(0o700);
            tokio::fs::set_permissions(&self.exports, private)
                .await
                .map_err(unavailable)?;
        }
        Ok(folder)
    }
}

impl Drop for MacPhotos {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.exports);
    }
}

#[async_trait]
impl PhotoLibrary for MacPhotos {
    async fn search(&self, query: &PhotoQuery, cx: &Ctx) -> Result<Vec<PhotoInfo>, ServiceError> {
        let (from, to, limit) = (unix(query.from), unix(query.to), query.limit.to_string());
        let found: Vec<Found> = run(
            &self.helper,
            &["search", "--from", &from, "--to", &to, "--limit", &limit],
            SEARCH_TIMEOUT,
            cx,
        )
        .await?;
        Ok(found.into_iter().map(Found::into_info).collect())
    }

    async fn export(
        &self,
        ids: &[String],
        max_pixels: u32,
        cx: &Ctx,
    ) -> Result<Vec<ExportedPhoto>, ServiceError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let folder = self.fresh_folder().await?;
        let folder_arg = folder.to_string_lossy();
        let max = max_pixels.to_string();
        let mut args = vec!["export", "--max", &max, "--dir", &folder_arg, "--"];
        args.extend(ids.iter().map(String::as_str));
        let exported: Vec<Exported> = run(&self.helper, &args, EXPORT_TIMEOUT, cx).await?;
        Ok(exported
            .into_iter()
            .map(|e| ExportedPhoto {
                id: e.id,
                path: e.path,
                bytes: e.bytes,
            })
            .collect())
    }
}

/// A window of numbered pictures, through the helper. One at a time: showing
/// new pictures closes the old window.
#[derive(Debug)]
pub struct MacShowcase {
    helper: PathBuf,
    title: String,
    window: Mutex<Option<Child>>,
}

impl MacShowcase {
    /// Uses `helper`, with windows titled `title`.
    #[must_use]
    pub fn new(helper: impl Into<PathBuf>, title: impl Into<String>) -> Self {
        Self {
            helper: helper.into(),
            title: title.into(),
            window: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Showcase for MacShowcase {
    async fn show(&self, pictures: &[PathBuf], cx: &Ctx) -> Result<(), ServiceError> {
        cx.check()?;
        if !self.helper.is_file() {
            return Err(ServiceError::NotSetUp(
                "the photos helper is missing".into(),
            ));
        }
        let mut window = self.window.lock().await;
        if let Some(mut old) = window.take() {
            let _ = old.kill().await;
        }
        let paths: Vec<String> = pictures
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let mut args = vec!["show", "--title", self.title.as_str(), "--"];
        args.extend(paths.iter().map(String::as_str));
        let mut child = command(&self.helper, &args)
            .spawn()
            .map_err(|e| ServiceError::Unavailable(format!("cannot start the viewer: {e}")))?;
        // It prints one line once the window is up, then keeps running.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ServiceError::Unavailable("the viewer has no output".into()))?;
        let mut line = String::new();
        let mut reader = tokio::io::BufReader::new(stdout);
        let read = tokio::time::timeout(SHOW_TIMEOUT, reader.read_line(&mut line)).await;
        match read {
            Ok(Ok(n)) if n > 0 && line.contains("showing") => {
                *window = Some(child);
                Ok(())
            }
            Ok(_) => {
                let _ = child.kill().await;
                Err(match serde_json::from_str::<Failure>(&line) {
                    Ok(failure) => to_error(failure.error),
                    Err(_) => ServiceError::Unavailable("the viewer did not start".into()),
                })
            }
            Err(_) => {
                let _ = child.kill().await;
                Err(ServiceError::Unavailable(
                    "the viewer took too long to start".into(),
                ))
            }
        }
    }

    async fn hide(&self, _cx: &Ctx) -> Result<(), ServiceError> {
        if let Some(mut child) = self.window.lock().await.take() {
            let _ = child.kill().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_failures_map_to_service_errors() {
        let failure = |code: &str| FailureBody {
            code: code.into(),
            message: "m".into(),
        };
        assert!(matches!(
            to_error(failure("PERMISSION_DENIED")),
            ServiceError::PermissionDenied(_)
        ));
        assert!(matches!(
            to_error(failure("NOT_FOUND")),
            ServiceError::Rejected(_)
        ));
        assert!(matches!(
            to_error(failure("EXPORT_FAILED")),
            ServiceError::Unavailable(_)
        ));
    }

    #[test]
    fn converts_found_photos() {
        let found: Vec<Found> = serde_json::from_str(
            r#"[{"id":"A/L0/001","taken":1786562373.5,"favorite":true,"lat":47.6,"lon":-122.3},
                {"id":"B/L0/001","taken":1786201225}]"#,
        )
        .unwrap();
        let infos: Vec<PhotoInfo> = found.into_iter().map(Found::into_info).collect();
        assert_eq!(
            infos[0].taken,
            UNIX_EPOCH + Duration::from_millis(1_786_562_373_500)
        );
        assert!(infos[0].favorite);
        assert_eq!(infos[0].location.unwrap().latitude, 47.6);
        assert_eq!(infos[1].location, None);
    }

    #[tokio::test]
    async fn a_missing_helper_says_it_needs_setting_up() {
        let photos = MacPhotos::new("/nonexistent/tsetsen-photos");
        let query = PhotoQuery {
            from: UNIX_EPOCH,
            to: SystemTime::now(),
            limit: 1,
        };
        let cx = Ctx::new(tsetsen_core::RequestId::new(1));
        assert!(matches!(
            photos.search(&query, &cx).await,
            Err(ServiceError::NotSetUp(_))
        ));
    }
}
