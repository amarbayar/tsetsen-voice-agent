//! Finding the user's files with Spotlight, which also matches the text
//! inside them: "the letter from the bank" finds a scan named `IMG_0042.pdf`
//! if its text says so.

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use jiff::Timestamp;
use tokio::process::Command;
use tsetsen_core::ports::{FileInfo, FileQuery, FileSearch};
use tsetsen_core::{Ctx, Interrupted, ServiceError};

const MDFIND: &str = "/usr/bin/mdfind";
const TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);
/// Paths looked at per search: enough for any real request.
const MAX_CANDIDATES: usize = 2_000;
/// Folders that are really packages: never searched inside.
const PACKAGES: &[&str] = &[
    "app",
    "photoslibrary",
    "musiclibrary",
    "bundle",
    "framework",
    "plugin",
];

/// Spotlight search over a few folders only.
#[derive(Clone, Debug)]
pub struct SpotlightFiles {
    roots: Vec<PathBuf>,
}

impl SpotlightFiles {
    /// Searches under `roots` only: typically Documents, Desktop and
    /// Downloads.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

/// A Spotlight query for `query`. Words keep letters and digits only, so
/// nothing in them can change the query's meaning.
pub(crate) fn spotlight_query(query: &FileQuery) -> String {
    let mut parts: Vec<String> = query
        .words
        .iter()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .map(|w| format!("(kMDItemFSName == \"*{w}*\"cd || kMDItemTextContent == \"{w}*\"cdw)"))
        .collect();
    let mut at = |op: &str, time: Option<SystemTime>| {
        if let Some(t) = time.and_then(|t| Timestamp::try_from(t).ok()) {
            let mut part = String::new();
            let _ = write!(
                part,
                "kMDItemFSContentChangeDate {op} $time.iso({})",
                t.strftime("%Y-%m-%dT%H:%M:%SZ")
            );
            parts.push(part);
        }
    };
    at(">=", query.from);
    at("<", query.to);
    if parts.is_empty() {
        parts.push("kMDItemFSName == \"*\"".into());
    }
    parts.join(" && ")
}

/// Whether `path` is the user's own file: not hidden, not inside a package.
fn own_file(path: &Path) -> bool {
    path.components().all(|c| match c {
        Component::Normal(name) => {
            let name = name.to_string_lossy().to_ascii_lowercase();
            !name.starts_with('.')
                && !name
                    .rsplit_once('.')
                    .is_some_and(|(_, e)| PACKAGES.contains(&e))
        }
        _ => true,
    })
}

async fn mdfind(roots: &[PathBuf], query: &str, cx: &Ctx) -> Result<String, ServiceError> {
    cx.check()?;
    let mut command = Command::new(MDFIND);
    for root in roots {
        command.arg("-onlyin").arg(root);
    }
    command
        .arg(query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = command.output();
    tokio::pin!(output);
    let (timeout, by_deadline) = match cx.remaining() {
        Some(left) if left < TIMEOUT => (left, true),
        _ => (TIMEOUT, false),
    };
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
                    ServiceError::Unavailable("Spotlight timed out".into())
                });
            }
            _ = tick.tick() => cx.check()?,
        }
    }
    .map_err(|e| ServiceError::NotSetUp(format!("cannot run Spotlight: {e}")))?;
    if !output.status.success() {
        return Err(ServiceError::Unavailable(
            "Spotlight could not search".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[async_trait]
impl FileSearch for SpotlightFiles {
    async fn search(&self, query: &FileQuery, cx: &Ctx) -> Result<Vec<FileInfo>, ServiceError> {
        if self.roots.is_empty() {
            return Err(ServiceError::NotSetUp("no folders to search".into()));
        }
        let listing = mdfind(&self.roots, &spotlight_query(query), cx).await?;
        let mut found: Vec<FileInfo> = listing
            .lines()
            .take(MAX_CANDIDATES)
            .map(PathBuf::from)
            .filter(|p| own_file(p))
            .filter_map(|path| {
                // Never follows links out of the user's folders.
                let meta = std::fs::symlink_metadata(&path).ok()?;
                meta.is_file().then(|| FileInfo {
                    modified: meta.modified().ok().unwrap_or(SystemTime::UNIX_EPOCH),
                    bytes: meta.len(),
                    path,
                })
            })
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.modified));
        found.truncate(query.limit);
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_queries_that_cannot_be_subverted() {
        let query = FileQuery {
            words: vec!["bank".into(), "x\" || kMDItemFSName == \"*".into()],
            from: Some(SystemTime::UNIX_EPOCH + Duration::from_hours(24)),
            to: None,
            limit: 5,
        };
        assert_eq!(
            spotlight_query(&query),
            "(kMDItemFSName == \"*bank*\"cd || kMDItemTextContent == \"bank*\"cdw) && \
             (kMDItemFSName == \"*xkMDItemFSName*\"cd || kMDItemTextContent == \"xkMDItemFSName*\"cdw) && \
             kMDItemFSContentChangeDate >= $time.iso(1970-01-02T00:00:00Z)"
        );
    }

    #[test]
    fn only_the_users_own_files() {
        assert!(own_file(Path::new("/Users/g/Documents/Bank letter.pdf")));
        assert!(!own_file(Path::new("/Users/g/Documents/.secret/a.pdf")));
        assert!(!own_file(Path::new(
            "/Users/g/Pictures/Photos Library.photoslibrary/x.jpg"
        )));
    }
}
