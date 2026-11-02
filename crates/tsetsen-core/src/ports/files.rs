use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;

use crate::{Ctx, ServiceError};

/// A file the user owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// Where it is.
    pub path: PathBuf,
    /// Last changed.
    pub modified: SystemTime,
    /// Its size.
    pub bytes: u64,
}

impl FileInfo {
    /// The file's name, e.g. `Tax return 2025.pdf`.
    #[must_use]
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }
}

/// Files whose name or text has every word, changed in `[from, to)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileQuery {
    /// Words to match, lowercase. Empty matches any file.
    pub words: Vec<String>,
    /// Changed at or after.
    pub from: Option<SystemTime>,
    /// Changed before.
    pub to: Option<SystemTime>,
    /// The most to return, newest first.
    pub limit: usize,
}

/// Finds the user's own files: documents, downloads, the desktop. Never
/// system or app files.
#[async_trait]
pub trait FileSearch: Send + Sync {
    /// Files matching `query`, newest first.
    async fn search(&self, query: &FileQuery, cx: &Ctx) -> Result<Vec<FileInfo>, ServiceError>;
}

/// Opens things the way double-clicking would.
#[async_trait]
pub trait Launcher: Send + Sync {
    /// Opens `path` in the app the user chose for it.
    async fn open(&self, path: &Path, cx: &Ctx) -> Result<(), ServiceError>;
}
