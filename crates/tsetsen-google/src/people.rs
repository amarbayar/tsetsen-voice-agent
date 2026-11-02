//! Finding people in the user's Google contacts, including the "other
//! contacts" Gmail collects from people they have written to.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::sync::OnceCell;
use tsetsen_core::ports::{Contact, Contacts};
use tsetsen_core::{Ctx, ServiceError};

use crate::auth::Auth;
use crate::http::send;

const PAGE: &str = "10";

/// Google Contacts, through the People API.
#[derive(Debug)]
pub struct GoogleContacts {
    auth: Arc<Auth>,
    warm: OnceCell<()>,
}

impl GoogleContacts {
    /// Searches the contacts of the account `auth` signs in to.
    #[must_use]
    pub fn new(auth: Arc<Auth>) -> Self {
        Self {
            auth,
            warm: OnceCell::new(),
        }
    }

    async fn search_in(
        &self,
        path: &str,
        query: &str,
        cx: &Ctx,
    ) -> Result<Vec<Contact>, ServiceError> {
        #[derive(Deserialize)]
        struct Results {
            #[serde(default)]
            results: Vec<Hit>,
        }
        #[derive(Deserialize)]
        struct Hit {
            person: Person,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Person {
            resource_name: String,
            #[serde(default)]
            names: Vec<Name>,
            #[serde(default)]
            email_addresses: Vec<Email>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Name {
            display_name: Option<String>,
        }
        #[derive(Deserialize)]
        struct Email {
            value: Option<String>,
        }

        let url = format!("{}/v1/{path}", self.auth.endpoints().people);
        for attempt in 0..2 {
            let token = self.auth.token(cx).await?;
            let request = self
                .auth
                .http()
                .get(&url)
                .bearer_auth(token.expose())
                .query(&[
                    ("query", query),
                    ("readMask", "names,emailAddresses"),
                    ("pageSize", PAGE),
                ])
                .send();
            let response = send(cx, request).await?.map_err(|e| {
                ServiceError::Unavailable(format!("Google Contacts is unreachable: {e}"))
            })?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.auth.invalidate().await;
                continue;
            }
            if !status.is_success() {
                return Err(match status {
                    StatusCode::FORBIDDEN => ServiceError::PermissionDenied(
                        "reading contacts is not allowed for this sign-in".into(),
                    ),
                    s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => {
                        ServiceError::Unavailable(format!("Google Contacts failed ({s})"))
                    }
                    s => ServiceError::Rejected(format!("Google Contacts refused ({s})")),
                });
            }
            let results: Results = response
                .json()
                .await
                .map_err(|e| ServiceError::Unavailable(format!("bad contacts response: {e}")))?;
            return Ok(results
                .results
                .into_iter()
                .filter_map(|hit| {
                    let emails: Vec<String> = hit
                        .person
                        .email_addresses
                        .into_iter()
                        .filter_map(|e| e.value)
                        .collect();
                    let name = hit
                        .person
                        .names
                        .into_iter()
                        .find_map(|n| n.display_name)
                        .or_else(|| emails.first().cloned())?;
                    Some(Contact {
                        id: hit.person.resource_name,
                        name,
                        emails,
                    })
                })
                .collect());
        }
        Err(ServiceError::NotSetUp(
            "Google no longer accepts the sign-in".into(),
        ))
    }
}

#[async_trait]
impl Contacts for GoogleContacts {
    async fn search(&self, query: &str, cx: &Ctx) -> Result<Vec<Contact>, ServiceError> {
        // Google asks for an empty search first, to warm its cache.
        self.warm
            .get_or_try_init(|| async {
                self.search_in("people:searchContacts", "", cx).await?;
                self.search_in("otherContacts:search", "", cx)
                    .await
                    .map(|_| ())
            })
            .await?;
        let mut found = self.search_in("people:searchContacts", query, cx).await?;
        for other in self.search_in("otherContacts:search", query, cx).await? {
            if !found
                .iter()
                .any(|c| c.emails.iter().any(|e| other.emails.contains(e)))
            {
                found.push(other);
            }
        }
        Ok(found)
    }
}
