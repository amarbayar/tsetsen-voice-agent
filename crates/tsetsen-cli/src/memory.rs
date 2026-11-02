//! Who the people in the user's life are, kept in one JSON file on this
//! computer, readable by the user only.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tsetsen_core::ports::{Contact, KnownPerson, PeopleMemory};
use tsetsen_core::{Ctx, ServiceError};

#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    relation: String,
    id: String,
    name: String,
    emails: Vec<String>,
}

/// [`PeopleMemory`] in a JSON file.
pub(crate) struct FileMemory {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileMemory {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    async fn read(&self) -> Result<Vec<Stored>, ServiceError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                ServiceError::Unavailable(format!("{} is damaged: {e}", self.path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(ServiceError::Unavailable(format!(
                "cannot read memory: {e}"
            ))),
        }
    }
}

#[async_trait]
impl PeopleMemory for FileMemory {
    async fn people(&self, cx: &Ctx) -> Result<Vec<KnownPerson>, ServiceError> {
        cx.check()?;
        let _guard = self.lock.lock().await;
        Ok(self
            .read()
            .await?
            .into_iter()
            .map(|s| KnownPerson {
                relation: s.relation,
                contact: Contact {
                    id: s.id,
                    name: s.name,
                    emails: s.emails,
                },
            })
            .collect())
    }

    async fn remember(&self, person: KnownPerson, cx: &Ctx) -> Result<(), ServiceError> {
        cx.check()?;
        let _guard = self.lock.lock().await;
        let mut stored = self.read().await?;
        stored.retain(|s| !(s.relation == person.relation && s.id == person.contact.id));
        stored.push(Stored {
            relation: person.relation,
            id: person.contact.id,
            name: person.contact.name,
            emails: person.contact.emails,
        });
        let json = serde_json::to_vec_pretty(&stored)
            .map_err(|e| ServiceError::Rejected(e.to_string()))?;
        let unavailable =
            |e: std::io::Error| ServiceError::Unavailable(format!("cannot save memory: {e}"));
        if let Some(dir) = self.path.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(unavailable)?;
        }
        // Write a private temporary file, then rename: never half a file.
        let temporary = self.path.with_extension("json.tmp");
        tokio::fs::write(&temporary, json)
            .await
            .map_err(unavailable)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                .await
                .map_err(unavailable)?;
        }
        tokio::fs::rename(&temporary, &self.path)
            .await
            .map_err(unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remembers_across_instances() {
        let dir = std::env::temp_dir().join(format!("tsetsen-memory-test-{}", std::process::id()));
        let path = dir.join("people.json");
        let cx = Ctx::new(tsetsen_core::RequestId::new(1));
        let david = KnownPerson {
            relation: "son".into(),
            contact: Contact {
                id: "people/c1".into(),
                name: "David Smith".into(),
                emails: vec!["david@example.com".into()],
            },
        };
        FileMemory::new(path.clone())
            .remember(david.clone(), &cx)
            .await
            .unwrap();
        FileMemory::new(path.clone())
            .remember(david.clone(), &cx)
            .await
            .unwrap();
        assert_eq!(FileMemory::new(path).people(&cx).await.unwrap(), [david]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
