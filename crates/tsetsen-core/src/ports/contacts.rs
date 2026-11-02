use async_trait::async_trait;

use crate::{Ctx, ServiceError};

/// Someone the user can reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    /// Stable within the address book that returned it.
    pub id: String,
    /// Display name, e.g. `David Smith`.
    pub name: String,
    /// Email addresses, the preferred one first.
    pub emails: Vec<String>,
}

/// The user's address book.
#[async_trait]
pub trait Contacts: Send + Sync {
    /// People whose name or address matches `query`, best match first.
    async fn search(&self, query: &str, cx: &Ctx) -> Result<Vec<Contact>, ServiceError>;
}

/// A person the user has told the assistant about: "David is my son".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownPerson {
    /// How they relate to the user, in the user's words: `son`, `doctor`.
    pub relation: String,
    /// Who they are.
    pub contact: Contact,
}

/// What the assistant remembers about the people in the user's life. Kept on
/// the device.
#[async_trait]
pub trait PeopleMemory: Send + Sync {
    /// Everyone remembered.
    async fn people(&self, cx: &Ctx) -> Result<Vec<KnownPerson>, ServiceError>;

    /// Remembers `person`, replacing anyone remembered with the same relation
    /// and the same contact.
    async fn remember(&self, person: KnownPerson, cx: &Ctx) -> Result<(), ServiceError>;
}
