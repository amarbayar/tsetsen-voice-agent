use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use tsetsen_core::ports::{FileInfo, FileQuery, FileSearch};
use tsetsen_core::{Ctx, ServiceError};

/// Folders deeper than this are not the user's own filing.
const MAX_DEPTH: usize = 6;
/// A bound on one search, so a huge folder cannot stall the conversation.
const MAX_VISITED: usize = 100_000;
/// How often a walk checks whether it was cancelled.
const CHECK_EVERY: usize = 512;
/// Folders that look like folders but are packages or caches: an app, a
/// photo library, a project's dependencies.
const SKIPPED: &[&str] = &[
    "app",
    "photoslibrary",
    "musiclibrary",
    "tvlibrary",
    "photolibrary",
    "bundle",
    "framework",
    "plugin",
    "xcodeproj",
    "xcworkspace",
    "git",
    "node_modules",
    "target",
];

/// Where people keep their own files, under `home`, if they exist.
#[must_use]
pub fn user_folders(home: &Path) -> Vec<PathBuf> {
    ["Documents", "Desktop", "Downloads"]
        .iter()
        .map(|f| home.join(f))
        .filter(|p| p.is_dir())
        .collect()
}

/// Finds files by name and date by walking folders.
#[derive(Clone, Debug)]
pub struct WalkFiles {
    roots: Vec<PathBuf>,
}

impl WalkFiles {
    /// Searches under `roots` only.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

fn skipped(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    if name.starts_with('.') {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    let extension = lower.rsplit_once('.').map_or("", |(_, e)| e);
    SKIPPED.contains(&lower.as_str()) || SKIPPED.contains(&extension)
}

fn matches(path: &Path, modified: SystemTime, query: &FileQuery) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    query.words.iter().all(|w| name.contains(&w.to_lowercase()))
        && query.from.is_none_or(|from| modified >= from)
        && query.to.is_none_or(|to| modified < to)
}

/// Walks `roots` breadth-first: nearer files are found before a deep
/// folder exhausts the budget.
fn walk(roots: &[PathBuf], query: &FileQuery, cx: &Ctx) -> Result<Vec<FileInfo>, ServiceError> {
    let mut found = Vec::new();
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        roots.iter().map(|r| (r.clone(), 0)).collect();
    let mut visited = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited % CHECK_EVERY == 0 {
                cx.check()?;
            }
            if visited > MAX_VISITED {
                tracing::debug!("file search stopped at its budget");
                return Ok(found);
            }
            let path = entry.path();
            if skipped(&path) {
                continue;
            }
            // Never follows links: they can lead out of the user's folders.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth + 1 < MAX_DEPTH {
                    queue.push_back((path, depth + 1));
                }
            } else if kind.is_file()
                && let Ok(meta) = entry.metadata()
                && let Ok(modified) = meta.modified()
                && matches(&path, modified, query)
            {
                found.push(FileInfo {
                    path,
                    modified,
                    bytes: meta.len(),
                });
            }
        }
    }
    Ok(found)
}

#[async_trait]
impl FileSearch for WalkFiles {
    async fn search(&self, query: &FileQuery, cx: &Ctx) -> Result<Vec<FileInfo>, ServiceError> {
        if self.roots.is_empty() {
            return Err(ServiceError::NotSetUp("no folders to search".into()));
        }
        let (roots, query_owned, cx_owned) = (self.roots.clone(), query.clone(), cx.clone());
        let mut found = tokio::task::spawn_blocking(move || walk(&roots, &query_owned, &cx_owned))
            .await
            .map_err(|e| ServiceError::Unavailable(format!("file search failed: {e}")))??;
        found.sort_by_key(|f| std::cmp::Reverse(f.modified));
        found.truncate(query.limit);
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_hidden_folders_and_packages() {
        for path in [
            "/h/.Trash",
            "/h/Pictures/Photos Library.photoslibrary",
            "/h/Calc.app",
            "/h/x/node_modules",
        ] {
            assert!(skipped(Path::new(path)), "{path}");
        }
        assert!(!skipped(Path::new("/h/Documents/Bank letter.pdf")));
    }
}
