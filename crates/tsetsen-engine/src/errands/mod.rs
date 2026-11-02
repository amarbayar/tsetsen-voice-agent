//! Things people do with a computer, as tools: find people, pictures and
//! files; show them; open them; share them.
//!
//! The tools are a few general verbs over kinds of things, not one tool per
//! scenario. Everything found goes into a shared [working set](items), so the
//! verbs compose: pictures found by `find_pictures` can be shown by `show` or
//! sent by `share`, and so can files found by `find_files`. A new kind of
//! thing works with every verb that accepts it, and a new verb works with
//! every kind it accepts.
//!
//! The tools never touch the OS. They use [`Capabilities`]: the services this
//! computer offers, such as an address book, a photo library or file search,
//! each behind a port. Each OS supplies its own adapters, and a tool is offered
//! only when the capabilities it needs are present. So the same tools run on
//! macOS, Windows and Linux, and adding support on a new OS means adding
//! adapters, not tools.
//!
//! Anything decided in code is decided here, not by the model: which days a
//! phrase means, who "my son" is once learned, exactly what will be sent.

mod files;
pub(crate) mod items;
mod people;
mod pictures;
mod share;
mod show;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::SystemTime;

use jiff::Zoned;
use serde::Deserialize;
use tsetsen_core::ServiceError;
use tsetsen_core::ports::{
    Contacts, Desktop, FileSearch, Launcher, Mailer, PeopleMemory, PhotoLibrary, Showcase,
    ToolCall, ToolSpec,
};

use crate::desktop_tools::DesktopTools;
use crate::tools::{DuplicateTool, ToolSet, Toolbox};
use crate::when::{self, Period};
use items::WorkingSet;
use people::People;

/// The most items shown at once: enough to choose from, few enough to see
/// clearly.
pub const MAX_SHOWN: usize = 12;
/// Gmail accepts 25 MB per message; base64 adds a third.
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 17 * 1024 * 1024;

/// The current time in the user's time zone.
pub type Clock = Arc<dyn Fn() -> Zoned + Send + Sync>;

/// The services this computer offers. Each OS fills in what it has; every
/// tool that can work with what is here is offered.
#[derive(Clone, Default)]
pub struct Capabilities {
    /// The address book.
    pub contacts: Option<Arc<dyn Contacts>>,
    /// Who people are to the user.
    pub memory: Option<Arc<dyn PeopleMemory>>,
    /// The photo library.
    pub photos: Option<Arc<dyn PhotoLibrary>>,
    /// Showing pictures and documents on screen.
    pub showcase: Option<Arc<dyn Showcase>>,
    /// The user's own files.
    pub files: Option<Arc<dyn FileSearch>>,
    /// Opening files in their apps.
    pub launcher: Option<Arc<dyn Launcher>>,
    /// Sending email.
    pub mail: Option<Arc<dyn Mailer>>,
    /// Operating apps on screen: the fallback for everything else.
    pub desktop: Option<Arc<dyn Desktop>>,
}

impl Capabilities {
    /// The names of what is present, for showing at startup.
    #[must_use]
    pub fn present(&self) -> Vec<&'static str> {
        [
            ("contacts", self.contacts.is_some()),
            ("memory", self.memory.is_some()),
            ("pictures", self.photos.is_some()),
            ("showing", self.showcase.is_some()),
            ("files", self.files.is_some()),
            ("opening", self.launcher.is_some()),
            ("email", self.mail.is_some()),
            ("apps", self.desktop.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, present)| present.then_some(name))
        .collect()
    }
}

/// What the errand tools share: the working set, the clock, and who people
/// are.
pub(crate) struct Shared {
    pub(crate) items: WorkingSet,
    pub(crate) clock: Clock,
    pub(crate) people: People,
    pub(crate) caps: Capabilities,
}

/// A toolbox for one area, if this computer can support it.
type Offer = fn(&Arc<Shared>) -> Option<Arc<dyn Toolbox>>;

/// Every area of errands. Adding one is a module and a line here.
const AREAS: &[Offer] = &[
    people::offer,
    pictures::offer,
    files::offer,
    show::offer,
    share::offer,
];

/// Every tool this computer supports: errands first, then operating apps on
/// screen as the fallback. `None` if there are none.
pub fn catalog(caps: Capabilities, clock: Clock) -> Result<Option<ToolSet>, DuplicateTool> {
    let shared = Arc::new(Shared {
        items: WorkingSet::default(),
        clock,
        people: People::new(caps.contacts.clone(), caps.memory.clone()),
        caps,
    });
    let mut boxes: Vec<Arc<dyn Toolbox>> =
        AREAS.iter().filter_map(|offer| offer(&shared)).collect();
    if !boxes.is_empty() {
        boxes.push(Arc::new(Found(Arc::clone(&shared))));
    }
    if let Some(desktop) = &shared.caps.desktop {
        boxes.push(Arc::new(DesktopTools::new(Arc::clone(desktop))));
    }
    if boxes.is_empty() {
        return Ok(None);
    }
    ToolSet::new(boxes).map(Some)
}

/// How many earlier finds a new task is told about.
const RECALLED: usize = 20;

/// No tools: what has been found so far, for new tasks to build on.
struct Found(Arc<Shared>);

#[async_trait::async_trait]
impl Toolbox for Found {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn assess(
        &self,
        call: &ToolCall,
        _: &tsetsen_core::Ctx,
    ) -> Result<crate::tools::Assessment, String> {
        Err(format!("There is no tool named `{}`.", call.name))
    }

    async fn run(&self, call: &ToolCall, _: &tsetsen_core::Ctx) -> crate::tools::ToolOutput {
        crate::tools::ToolOutput::failed(format!("There is no tool named `{}`.", call.name))
    }

    fn context(&self) -> Option<String> {
        let recent = self.0.items.recent(RECALLED);
        (!recent.is_empty()).then(|| format!("Found earlier, usable by id: {}.", recent.join("; ")))
    }
}

pub(crate) fn spec(name: &str, description: &str, parameters: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: parameters.into(),
    }
}

pub(crate) fn args<T: for<'de> Deserialize<'de>>(call: &ToolCall) -> Result<T, String> {
    serde_json::from_str(&call.arguments)
        .map_err(|e| format!("Bad arguments for {}: {e}", call.name))
}

/// A list of handles, however the model wrote it: `["p1","p2"]` or `"p1, p2"`.
#[derive(Deserialize, Default)]
#[serde(untagged)]
pub(crate) enum Handles {
    One(String),
    Many(Vec<String>),
    #[default]
    #[serde(skip)]
    None,
}

impl Handles {
    pub(crate) fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => s
                .split(',')
                .map(|p| p.trim().to_owned())
                .filter(|p| !p.is_empty())
                .collect(),
            Self::Many(v) => v.into_iter().map(|h| h.trim().to_owned()).collect(),
            Self::None => Vec::new(),
        }
    }
}

/// A service error, explained for the model.
pub(crate) fn explain(error: &ServiceError, what: &str) -> String {
    match error {
        ServiceError::NotSetUp(why) => format!(
            "{what} is not set up on this computer ({why}). Tell the user simply that this \
             needs to be connected first, and that a family member can help."
        ),
        ServiceError::PermissionDenied(why) => {
            format!("This computer has not allowed access to {what} ({why}). Tell the user simply.")
        }
        ServiceError::Unavailable(why) => {
            format!("{what} could not be reached right now ({why}). Suggest trying again soon.")
        }
        ServiceError::Rejected(why) => format!("{what} refused this: {why}"),
        ServiceError::Uncertain(why) => format!(
            "Something went wrong and it may or may not have happened ({why}). Do not try \
             again; tell the user it may not have worked."
        ),
        ServiceError::Interrupted(_) => "Interrupted.".into(),
        _ => format!("{what} failed: {error}"),
    }
}

pub(crate) fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `david@example.com` as it is said: "david at example dot com".
#[must_use]
pub fn spoken_address(address: &str) -> String {
    address.replace('@', " at ").replace('.', " dot ")
}

/// The periods `phrase` may mean, with their bounds in the user's time zone,
/// most likely first. No phrase means `default`.
pub(crate) fn periods(
    phrase: Option<&str>,
    now: &Zoned,
    default: &str,
) -> Result<Vec<(Period, SystemTime, SystemTime)>, String> {
    let today = now.date();
    let phrase = phrase.map(str::trim).filter(|p| !p.is_empty());
    let alternatives = match phrase {
        Some(phrase) => when::resolve(phrase, today).ok_or_else(|| {
            format!(
                "Could not tell which day \"{phrase}\" means. Ask the user, or pass a date like \
                 {today}."
            )
        })?,
        None => when::resolve(default, today).unwrap_or_default(),
    };
    let tz = now.time_zone();
    Ok(alternatives
        .into_iter()
        .filter_map(|period| {
            let start = period.first.to_zoned(tz.clone()).ok()?;
            let end = period.last.tomorrow().ok()?.to_zoned(tz.clone()).ok()?;
            Some((period, start.timestamp().into(), end.timestamp().into()))
        })
        .collect())
}

/// A file name as it is said: `Tax return 2025.pdf` → "Tax return 2025".
pub(crate) fn spoken_file(name: &str) -> String {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    stem.replace(['_', '-'], " ")
}

/// Appends "and N more" when a list was cut.
pub(crate) fn more(out: &mut String, total: usize, shown: usize) {
    if total > shown {
        let _ = writeln!(out, "and {} more.", total - shown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_addresses_and_file_names() {
        assert_eq!(
            spoken_address("david.smith@example.com"),
            "david dot smith at example dot com"
        );
        assert_eq!(spoken_file("Tax_return-2025.pdf"), "Tax return 2025");
        assert_eq!(spoken_file("notes"), "notes");
    }

    #[test]
    fn reads_handles_however_written() {
        let parse = |json: &str| serde_json::from_str::<Handles>(json).unwrap().into_vec();
        assert_eq!(parse(r#"["p1"," p2"]"#), ["p1", "p2"]);
        assert_eq!(parse(r#""p1, f2""#), ["p1", "f2"]);
        assert!(parse(r#""""#).is_empty());
    }

    #[test]
    fn offers_only_what_the_computer_supports() {
        let clock: Clock = Arc::new(Zoned::now);
        assert!(
            catalog(Capabilities::default(), Arc::clone(&clock))
                .unwrap()
                .is_none()
        );
        let memory: Arc<dyn PeopleMemory> = Arc::new(tsetsen_testkit_free::NoMemory);
        let caps = Capabilities {
            memory: Some(memory),
            ..Capabilities::default()
        };
        let names: Vec<String> = catalog(caps, clock)
            .unwrap()
            .unwrap()
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            ["find_person", "remember_person"],
            "people only: nothing to share with"
        );
    }

    /// The engine's unit tests cannot use the testkit (it depends on the
    /// engine's ports only, but keeping unit tests self-contained is simpler).
    mod tsetsen_testkit_free {
        use async_trait::async_trait;
        use tsetsen_core::ports::{KnownPerson, PeopleMemory};
        use tsetsen_core::{Ctx, ServiceError};

        pub(super) struct NoMemory;

        #[async_trait]
        impl PeopleMemory for NoMemory {
            async fn people(&self, _: &Ctx) -> Result<Vec<KnownPerson>, ServiceError> {
                Ok(Vec::new())
            }
            async fn remember(&self, _: KnownPerson, _: &Ctx) -> Result<(), ServiceError> {
                Ok(())
            }
        }
    }
}
