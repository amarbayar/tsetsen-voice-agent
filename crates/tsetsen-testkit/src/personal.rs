//! Fakes for personal-data services: contacts, memory, photos, files, mail.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tsetsen_core::ports::{
    Contact, Contacts, ExportedPhoto, FileInfo, FileQuery, FileSearch, KnownPerson, Launcher,
    Mailer, OutgoingMail, PeopleMemory, PhotoInfo, PhotoLibrary, PhotoQuery, SentMail, Showcase,
};
use tsetsen_core::{Ctx, ServiceError};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A contact with one address.
#[must_use]
pub fn contact(id: &str, name: &str, email: &str) -> Contact {
    Contact {
        id: id.into(),
        name: name.into(),
        emails: vec![email.into()],
    }
}

/// An address book that matches every query word against names and
/// addresses.
#[derive(Debug, Default)]
pub struct FakeContacts {
    people: Vec<Contact>,
    queries: Mutex<Vec<String>>,
}

impl FakeContacts {
    /// An address book holding `people`.
    #[must_use]
    pub fn new(people: Vec<Contact>) -> Self {
        Self {
            people,
            queries: Mutex::default(),
        }
    }

    /// Every query, in order.
    pub fn queries(&self) -> Vec<String> {
        lock(&self.queries).clone()
    }
}

#[async_trait]
impl Contacts for FakeContacts {
    async fn search(&self, query: &str, cx: &Ctx) -> Result<Vec<Contact>, ServiceError> {
        cx.check()?;
        lock(&self.queries).push(query.into());
        let query = query.to_lowercase();
        Ok(self
            .people
            .iter()
            .filter(|c| {
                query.split_whitespace().all(|w| {
                    c.name.to_lowercase().contains(w) || c.emails.iter().any(|e| e.contains(w))
                })
            })
            .cloned()
            .collect())
    }
}

/// Memory of people, held in memory.
#[derive(Debug, Default)]
pub struct FakeMemory {
    people: Mutex<Vec<KnownPerson>>,
}

impl FakeMemory {
    /// Remembering `people` already.
    #[must_use]
    pub fn new(people: Vec<KnownPerson>) -> Self {
        Self {
            people: Mutex::new(people),
        }
    }

    /// Everyone remembered now.
    pub fn people_now(&self) -> Vec<KnownPerson> {
        lock(&self.people).clone()
    }
}

#[async_trait]
impl PeopleMemory for FakeMemory {
    async fn people(&self, cx: &Ctx) -> Result<Vec<KnownPerson>, ServiceError> {
        cx.check()?;
        Ok(self.people_now())
    }

    async fn remember(&self, person: KnownPerson, cx: &Ctx) -> Result<(), ServiceError> {
        cx.check()?;
        let mut people = lock(&self.people);
        people.retain(|p| !(p.relation == person.relation && p.contact.id == person.contact.id));
        people.push(person);
        Ok(())
    }
}

/// A photo taken `seconds` after the Unix epoch.
#[must_use]
pub fn photo_at(id: &str, seconds: u64) -> PhotoInfo {
    PhotoInfo {
        id: id.into(),
        taken: SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
        location: None,
        favorite: false,
    }
}

/// A photo library with fixed photos. Exports are pretend files of
/// `bytes_each` bytes under `/fake/`.
#[derive(Debug)]
pub struct FakePhotos {
    photos: Vec<PhotoInfo>,
    bytes_each: u64,
    exported: Mutex<Vec<Vec<String>>>,
}

impl FakePhotos {
    /// A library of `photos`, exporting 400 kB each.
    #[must_use]
    pub fn new(photos: Vec<PhotoInfo>) -> Self {
        Self {
            photos,
            bytes_each: 400_000,
            exported: Mutex::default(),
        }
    }

    /// Exports of `bytes` each instead.
    #[must_use]
    pub fn with_export_size(mut self, bytes: u64) -> Self {
        self.bytes_each = bytes;
        self
    }

    /// The ids of every export, per call.
    pub fn exports(&self) -> Vec<Vec<String>> {
        lock(&self.exported).clone()
    }
}

#[async_trait]
impl PhotoLibrary for FakePhotos {
    async fn search(&self, query: &PhotoQuery, cx: &Ctx) -> Result<Vec<PhotoInfo>, ServiceError> {
        cx.check()?;
        let mut found: Vec<PhotoInfo> = self
            .photos
            .iter()
            .filter(|p| p.taken >= query.from && p.taken < query.to)
            .cloned()
            .collect();
        found.sort_by_key(|p| std::cmp::Reverse(p.taken));
        found.truncate(query.limit);
        Ok(found)
    }

    async fn export(
        &self,
        ids: &[String],
        _max_pixels: u32,
        cx: &Ctx,
    ) -> Result<Vec<ExportedPhoto>, ServiceError> {
        cx.check()?;
        lock(&self.exported).push(ids.to_vec());
        Ok(ids
            .iter()
            .map(|id| ExportedPhoto {
                id: id.clone(),
                path: PathBuf::from(format!("/fake/{id}.jpg")),
                bytes: self.bytes_each,
            })
            .collect())
    }
}

/// Records what was shown.
#[derive(Debug, Default)]
pub struct FakeShowcase {
    shown: Mutex<Option<Vec<PathBuf>>>,
}

impl FakeShowcase {
    /// What is on screen now, if anything.
    pub fn showing(&self) -> Option<Vec<PathBuf>> {
        lock(&self.shown).clone()
    }
}

#[async_trait]
impl Showcase for FakeShowcase {
    async fn show(&self, pictures: &[PathBuf], cx: &Ctx) -> Result<(), ServiceError> {
        cx.check()?;
        *lock(&self.shown) = Some(pictures.to_vec());
        Ok(())
    }

    async fn hide(&self, _cx: &Ctx) -> Result<(), ServiceError> {
        *lock(&self.shown) = None;
        Ok(())
    }
}

/// Records sent mail instead of sending it.
#[derive(Debug, Default)]
pub struct FakeMailer {
    sent: Mutex<Vec<OutgoingMail>>,
}

impl FakeMailer {
    /// Everything sent, in order.
    pub fn sent(&self) -> Vec<OutgoingMail> {
        lock(&self.sent).clone()
    }
}

#[async_trait]
impl Mailer for FakeMailer {
    async fn send(&self, mail: &OutgoingMail, cx: &Ctx) -> Result<SentMail, ServiceError> {
        cx.check()?;
        let mut sent = lock(&self.sent);
        sent.push(mail.clone());
        Ok(SentMail {
            id: format!("m{}", sent.len()),
        })
    }
}

/// A file of `bytes` bytes at `path`, changed `seconds` after the epoch.
#[must_use]
pub fn file_at(path: &str, seconds: u64, bytes: u64) -> FileInfo {
    FileInfo {
        path: path.into(),
        modified: SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
        bytes,
    }
}

/// File search over a fixed list, matching every word against file names.
#[derive(Debug, Default)]
pub struct FakeFiles {
    files: Vec<FileInfo>,
    queries: Mutex<Vec<FileQuery>>,
}

impl FakeFiles {
    /// Search over `files`.
    #[must_use]
    pub fn new(files: Vec<FileInfo>) -> Self {
        Self {
            files,
            queries: Mutex::default(),
        }
    }

    /// Every query so far.
    pub fn queries(&self) -> Vec<FileQuery> {
        lock(&self.queries).clone()
    }
}

#[async_trait]
impl FileSearch for FakeFiles {
    async fn search(&self, query: &FileQuery, _: &Ctx) -> Result<Vec<FileInfo>, ServiceError> {
        lock(&self.queries).push(query.clone());
        let mut found: Vec<FileInfo> = self
            .files
            .iter()
            .filter(|f| {
                let name = f.name().to_lowercase();
                query.words.iter().all(|w| name.contains(w.as_str()))
                    && query.from.is_none_or(|from| f.modified >= from)
                    && query.to.is_none_or(|to| f.modified < to)
            })
            .cloned()
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.modified));
        found.truncate(query.limit);
        Ok(found)
    }
}

/// Records what it was asked to open.
#[derive(Debug, Default)]
pub struct FakeLauncher {
    opened: Mutex<Vec<PathBuf>>,
}

impl FakeLauncher {
    /// Everything opened so far.
    pub fn opened(&self) -> Vec<PathBuf> {
        lock(&self.opened).clone()
    }
}

#[async_trait]
impl Launcher for FakeLauncher {
    async fn open(&self, path: &Path, _: &Ctx) -> Result<(), ServiceError> {
        lock(&self.opened).push(path.to_owned());
        Ok(())
    }
}
