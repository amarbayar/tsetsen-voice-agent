//! The user's own files: finding them by what they are about and when, and
//! opening them.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::Deserialize;
use tsetsen_core::Ctx;
use tsetsen_core::ports::{FileQuery, FileSearch, ToolCall, ToolSpec};
use tsetsen_policy::Consequence;

use super::items::Item;
use super::show::previewable;
use super::{Shared, args, explain, more, periods, plural, spec, spoken_file};
use crate::tools::{Assessment, ToolOutput, Toolbox};
use crate::when::day_label;

const FIND_FILES: &str = "find_files";
const OPEN: &str = "open";
/// The most files offered at once: more is too many to choose from by ear.
const MAX_FILES: usize = 8;
/// What opening would run as a program rather than show as a document. A
/// file found by its name could be anything, so these are never opened.
const RUNNABLE: &[&str] = &[
    "app",
    "command",
    "sh",
    "zsh",
    "bash",
    "py",
    "rb",
    "pl",
    "js",
    "jar",
    "scpt",
    "applescript",
    "workflow",
    "pkg",
    "mpkg",
    "dmg",
    "terminal",
    "exe",
    "msi",
    "bat",
    "cmd",
    "com",
    "ps1",
    "vbs",
    "lnk",
    "scr",
    "desktop",
    "appimage",
    "run",
    "bin",
    "deb",
    "rpm",
    "url",
    "webloc",
    "inetloc",
];

/// Whether opening `path` would only show it.
fn openable(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_none_or(|e| !RUNNABLE.contains(&e.to_ascii_lowercase().as_str()))
}

/// Words that say nothing about which file: "the letter from the bank" is
/// about "letter" and "bank".
const FILLER: &[&str] = &[
    "a",
    "an",
    "the",
    "my",
    "our",
    "your",
    "from",
    "of",
    "to",
    "for",
    "about",
    "with",
    "on",
    "in",
    "that",
    "this",
    "these",
    "those",
    "some",
    "file",
    "files",
    "document",
    "documents",
    "thing",
    "one",
    "it",
    "me",
    "i",
    "we",
    "got",
    "sent",
    "had",
    "have",
    "was",
    "is",
];

/// Finding and opening files.
pub(crate) struct FileTools {
    shared: Arc<Shared>,
    search: Arc<dyn FileSearch>,
}

pub(crate) fn offer(shared: &Arc<Shared>) -> Option<Arc<dyn Toolbox>> {
    let search = shared.caps.files.clone()?;
    Some(Arc::new(FileTools {
        shared: Arc::clone(shared),
        search,
    }))
}

#[derive(Deserialize)]
struct FindArgs {
    #[serde(default, alias = "query", alias = "words", alias = "name")]
    about: Option<String>,
    #[serde(default, alias = "date", alias = "time")]
    when: Option<String>,
}

#[derive(Deserialize)]
struct OpenArgs {
    #[serde(alias = "items", alias = "id")]
    item: String,
}

/// The words of `about` that can tell files apart.
fn keywords(about: &str) -> Vec<String> {
    about
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1 && !FILLER.contains(w))
        .map(str::to_owned)
        .collect()
}

impl FileTools {
    async fn find(&self, about: Option<&str>, when: Option<&str>, cx: &Ctx) -> ToolOutput {
        let now = (self.shared.clock)();
        let words = about.map(keywords).unwrap_or_default();
        // Every period the words may mean: a file changed in any of them.
        let range = match when.map(str::trim).filter(|w| !w.is_empty()) {
            Some(phrase) => match periods(Some(phrase), &now, "") {
                Ok(found) if !found.is_empty() => Some((
                    found.iter().map(|(_, from, _)| *from).min(),
                    found.iter().map(|(_, _, to)| *to).max(),
                    found[0].0.label.clone(),
                )),
                Ok(_) => None,
                Err(problem) => return ToolOutput::failed(problem),
            },
            None => None,
        };
        if words.is_empty() && range.is_none() {
            return ToolOutput::failed(
                "Say what the file is about or when it was from, in the user's words.",
            );
        }
        let query = FileQuery {
            words: words.clone(),
            from: range.as_ref().and_then(|r| r.0),
            to: range.as_ref().and_then(|r| r.1),
            limit: 50,
        };
        let found = match self.search.search(&query, cx).await {
            Ok(found) => found,
            Err(error) => return ToolOutput::failed(explain(&error, "Searching files")),
        };
        let what = match (&range, words.is_empty()) {
            (Some((_, _, label)), true) => format!("from {label}"),
            (Some((_, _, label)), false) => format!("about {} from {label}", words.join(" ")),
            (None, _) => format!("about {}", words.join(" ")),
        };
        if found.is_empty() {
            return ToolOutput::ok(format!(
                "No files {what}. Ask the user for another word that might be in its name, or \
                 roughly when they got it."
            ));
        }
        let today = now.date();
        let mut out = format!("Found {} {what}:\n", plural(found.len(), "file", "files"));
        for file in found.iter().take(MAX_FILES) {
            let changed = Timestamp::try_from(file.modified)
                .map(|t| day_label(t.to_zoned(now.time_zone().clone()).date(), today))
                .unwrap_or_default();
            let name = file.name();
            let handle = self.shared.items.add(Item::File(file.clone()));
            let _ = writeln!(
                out,
                "{handle}: \"{}\" ({name}), changed {changed}",
                spoken_file(&name)
            );
        }
        more(&mut out, found.len(), MAX_FILES);
        let can_show =
            self.shared.caps.showcase.is_some() && found.iter().any(|f| previewable(&f.path));
        out.push_str(if can_show {
            "To let the user see one, call show; to open one, call open."
        } else {
            "To open one, call open."
        });
        ToolOutput::ok(out)
    }

    fn target(&self, call: &ToolCall) -> Result<(String, std::path::PathBuf), String> {
        let OpenArgs { item } = args(call)?;
        let found = self
            .shared
            .items
            .get(&item)
            .ok_or_else(|| self.shared.items.unknown(&item))?;
        let (path, _) = found.file().ok_or_else(|| {
            format!("`{item}` is a person; only files and pictures can be opened.")
        })?;
        if !openable(&path) {
            return Err(format!(
                "`{item}` is a program, not a document; it is never opened."
            ));
        }
        let name = path
            .file_name()
            .map_or_else(String::new, |n| spoken_file(&n.to_string_lossy()));
        Ok((name, path))
    }
}

#[async_trait]
impl Toolbox for FileTools {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![spec(
            FIND_FILES,
            "Find the user's own files (documents, downloads, letters, forms) by what they are \
             about and roughly when, in the user's words. Returns ids like f1 that other tools \
             take.",
            r#"{"type":"object","properties":{"about":{"type":"string","description":"What it is about, e.g. the letter from the bank, tax return"},"when":{"type":"string","description":"Roughly when, e.g. last week, in March"}}}"#,
        )];
        if self.shared.caps.launcher.is_some() {
            specs.push(spec(
                OPEN,
                "Open a file or picture the way double-clicking it would.",
                r#"{"type":"object","properties":{"item":{"type":"string","description":"An id like f1 or p2"}},"required":["item"]}"#,
            ));
        }
        specs
    }

    async fn assess(&self, call: &ToolCall, _cx: &Ctx) -> Result<Assessment, String> {
        match call.name.as_str() {
            FIND_FILES => {
                let FindArgs { about, when } = args(call)?;
                let what = [about, when]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(Assessment {
                    summary: format!("look for files: {what}"),
                    consequence: Consequence::Observe,
                    progress: Some("Looking through your files".into()),
                })
            }
            OPEN => {
                let (name, _) = self.target(call)?;
                Ok(Assessment {
                    summary: format!("open \"{name}\""),
                    consequence: Consequence::Reversible,
                    progress: Some(format!("Opening {name}")),
                })
            }
            other => Err(format!("There is no tool named `{other}`.")),
        }
    }

    async fn run(&self, call: &ToolCall, cx: &Ctx) -> ToolOutput {
        match call.name.as_str() {
            FIND_FILES => match args::<FindArgs>(call) {
                Ok(FindArgs { about, when }) => {
                    self.find(about.as_deref(), when.as_deref(), cx).await
                }
                Err(problem) => ToolOutput::failed(problem),
            },
            OPEN => {
                let Some(launcher) = &self.shared.caps.launcher else {
                    return ToolOutput::failed("Opening files is not available.");
                };
                let (name, path) = match self.target(call) {
                    Ok(target) => target,
                    Err(problem) => return ToolOutput::failed(problem),
                };
                match launcher.open(&path, cx).await {
                    Ok(()) => ToolOutput::ok(format!("\"{name}\" is open on the screen.")),
                    Err(error) => ToolOutput::failed(explain(&error, "Opening it")),
                }
            }
            other => ToolOutput::failed(format!("There is no tool named `{other}`.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_words_that_tell_files_apart() {
        assert_eq!(keywords("the letter from the bank"), ["letter", "bank"]);
        assert_eq!(keywords("my 2025 tax return"), ["2025", "tax", "return"]);
        assert!(keywords("that document").is_empty());
    }

    #[test]
    fn never_opens_programs() {
        for path in ["/d/Letter.pdf", "/d/notes", "/d/Photo.HEIC"] {
            assert!(openable(std::path::Path::new(path)), "{path}");
        }
        for path in [
            "/d/Install.pkg",
            "/d/run.COMMAND",
            "/d/x.app",
            "/d/setup.exe",
            "/d/a.webloc",
        ] {
            assert!(!openable(std::path::Path::new(path)), "{path}");
        }
    }
}
