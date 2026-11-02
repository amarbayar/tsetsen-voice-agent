//! The things a conversation is about, shared by every tool.
//!
//! Whatever a tool finds (a person, a picture, a file) goes into one working
//! set under a short handle: `c1` for a person, `p3` for a picture, `f2` for a
//! file. Any tool can take any handle it understands, so finding and acting
//! compose freely: pictures found by one tool are shared by another, and a
//! new kind of thing works with every verb that accepts it. Handles are never
//! reused, so a stale one can never point at something else.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use tsetsen_core::ports::{Contact, ExportedPhoto, FileInfo, PhotoInfo};

/// One thing in the working set.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Item {
    /// Someone to contact.
    Person {
        contact: Contact,
        /// Who they are to the user, if known: `son`.
        relation: Option<String>,
    },
    /// A picture from the library, with its exported copy.
    Picture {
        info: PhotoInfo,
        file: ExportedPhoto,
    },
    /// One of the user's files.
    File(FileInfo),
}

/// The kinds of things in the working set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Person,
    Picture,
    File,
}

impl Kind {
    /// The letter that starts this kind's handles.
    fn prefix(self) -> char {
        match self {
            Self::Person => 'c',
            Self::Picture => 'p',
            Self::File => 'f',
        }
    }
}

impl Item {
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Self::Person { .. } => Kind::Person,
            Self::Picture { .. } => Kind::Picture,
            Self::File(_) => Kind::File,
        }
    }

    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Person { contact: a, .. }, Self::Person { contact: b, .. }) => {
                a.id == b.id || a.emails.iter().any(|e| b.emails.contains(e))
            }
            (Self::Picture { file: a, .. }, Self::Picture { file: b, .. }) => a.path == b.path,
            (Self::File(a), Self::File(b)) => a.path == b.path,
            _ => false,
        }
    }

    /// What it is, in a few words, for the model.
    fn describe(&self) -> String {
        match self {
            Self::Person { contact, relation } => match relation {
                Some(relation) => format!("{}, the user's {relation}", contact.name),
                None => contact.name.clone(),
            },
            Self::Picture { info, .. } => {
                let taken = jiff::Timestamp::try_from(info.taken)
                    .map(|t| t.strftime("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                format!("a picture taken {taken}")
            }
            Self::File(info) => format!("the file \"{}\"", info.name()),
        }
    }

    /// A file to send or show, with its size, if this is one.
    pub(crate) fn file(&self) -> Option<(PathBuf, u64)> {
        match self {
            Self::Picture { file, .. } => Some((file.path.clone(), file.bytes)),
            Self::File(info) => Some((info.path.clone(), info.bytes)),
            Self::Person { .. } => None,
        }
    }
}

#[derive(Default)]
struct Inner {
    items: HashMap<String, Item>,
    /// Handles in the order they were first given.
    order: Vec<String>,
    counters: HashMap<char, usize>,
}

/// Everything found so far in the conversation.
#[derive(Default)]
pub(crate) struct WorkingSet(Mutex<Inner>);

/// How many ids an unknown-id error lists.
const UNKNOWN_LISTS: usize = 12;

impl WorkingSet {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `item` and returns its handle. The same thing found again keeps
    /// its handle, updated.
    pub(crate) fn add(&self, item: Item) -> String {
        let mut inner = self.inner();
        if let Some(handle) = inner
            .items
            .iter()
            .find(|(_, known)| known.same(&item))
            .map(|(h, _)| h.clone())
        {
            let known = inner.items.get_mut(&handle);
            if let Some(known) = known {
                // Keep a relation learned earlier if the new copy lacks one.
                let keep = match (&*known, &item) {
                    (
                        Item::Person {
                            relation: Some(r), ..
                        },
                        Item::Person { relation: None, .. },
                    ) => Some(r.clone()),
                    _ => None,
                };
                *known = item;
                if let (Some(r), Item::Person { relation, .. }) = (keep, known) {
                    *relation = Some(r);
                }
            }
            return handle;
        }
        let prefix = item.kind().prefix();
        let counter = inner.counters.entry(prefix).or_default();
        *counter += 1;
        let handle = format!("{prefix}{counter}");
        inner.items.insert(handle.clone(), item);
        inner.order.push(handle.clone());
        handle
    }

    /// The item behind `handle`.
    pub(crate) fn get(&self, handle: &str) -> Option<Item> {
        self.inner().items.get(handle.trim()).cloned()
    }

    /// Records who a person is to the user.
    pub(crate) fn set_relation(&self, handle: &str, relation: &str) {
        if let Some(Item::Person { relation: r, .. }) = self.inner().items.get_mut(handle.trim()) {
            *r = Some(relation.to_owned());
        }
    }

    /// The latest `max` things found, oldest first, each as "c1: David
    /// Smith, the user's son".
    pub(crate) fn recent(&self, max: usize) -> Vec<String> {
        let inner = self.inner();
        let start = inner.order.len().saturating_sub(max);
        inner.order[start..]
            .iter()
            .filter_map(|h| Some(format!("{h}: {}", inner.items.get(h)?.describe())))
            .collect()
    }

    /// Whether anything of `kind` has been found.
    /// Why `handle` cannot be used, with the ids that can: a model that
    /// guessed an id recovers in one step.
    pub(crate) fn unknown(&self, handle: &str) -> String {
        let recent = self.recent(UNKNOWN_LISTS);
        if recent.is_empty() {
            format!("Unknown id `{handle}`: nothing has been found yet. Find it first.")
        } else {
            format!(
                "Unknown id `{handle}`. Use only these ids, found so far: {}.",
                recent.join("; ")
            )
        }
    }

    pub(crate) fn has(&self, kind: Kind) -> bool {
        self.inner().items.values().any(|i| i.kind() == kind)
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn person(id: &str, email: &str) -> Item {
        Item::Person {
            contact: Contact {
                id: id.into(),
                name: "David".into(),
                emails: vec![email.into()],
            },
            relation: None,
        }
    }

    #[test]
    fn handles_are_typed_stable_and_never_reused() {
        let set = WorkingSet::default();
        assert_eq!(set.add(person("a", "d@x.com")), "c1");
        let file = Item::File(FileInfo {
            path: "/home/letter.pdf".into(),
            modified: SystemTime::UNIX_EPOCH,
            bytes: 10,
        });
        assert_eq!(set.add(file.clone()), "f1");
        assert_eq!(set.add(person("b", "e@x.com")), "c2");
        set.set_relation("c1", "son");
        // Found again: same handle, relation kept.
        assert_eq!(set.add(person("a", "d@x.com")), "c1");
        assert!(
            matches!(set.get("c1"), Some(Item::Person { relation: Some(r), .. }) if r == "son")
        );
        assert_eq!(set.add(file), "f1");
        assert_eq!(set.get("c9"), None);
        assert_eq!(
            set.recent(2),
            ["f1: the file \"letter.pdf\"", "c2: David"],
            "latest last, each once"
        );
        assert!(set.has(Kind::Person));
        assert!(set.has(Kind::File));
        assert!(!set.has(Kind::Picture));
    }
}
