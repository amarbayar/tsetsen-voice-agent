//! People: who "my son" or "David" is, for every tool that involves someone.
//!
//! [`People`] resolves the user's words to contacts, from what it remembers
//! first and the address book second, and learns who someone is when the
//! user confirms it, whichever tool they were using at the time.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde::Deserialize;
use tsetsen_core::ports::{Contact, Contacts, KnownPerson, PeopleMemory, ToolCall, ToolSpec};
use tsetsen_core::{Ctx, ServiceError};
use tsetsen_policy::Consequence;

use super::items::Item;
use super::{Shared, args, explain, more, spec};
use crate::tools::{Assessment, ToolOutput, Toolbox};

const FIND_PERSON: &str = "find_person";
const REMEMBER_PERSON: &str = "remember_person";
/// The most people offered for one name.
const MAX_PEOPLE: usize = 5;

/// Words for the people in someone's life, singular.
const RELATIONS: &[&str] = &[
    "son",
    "daughter",
    "grandson",
    "granddaughter",
    "grandchild",
    "grandkid",
    "child",
    "kid",
    "husband",
    "wife",
    "partner",
    "brother",
    "sister",
    "mother",
    "mom",
    "mum",
    "father",
    "dad",
    "niece",
    "nephew",
    "aunt",
    "uncle",
    "cousin",
    "friend",
    "neighbor",
    "neighbour",
    "doctor",
    "nurse",
    "dentist",
    "pastor",
    "priest",
    "caregiver",
    "carer",
    "lawyer",
    "accountant",
];

/// The relation `who` names, singular: "my son" → `son`, "the grandkids" →
/// `grandkid`.
pub(crate) fn relation(who: &str) -> Option<String> {
    let lowered = who.to_lowercase().replace(['’', '\''], "");
    lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .find_map(|word| {
            let singular = match word {
                "children" => "child",
                "grandchildren" => "grandchild",
                other => other
                    .strip_suffix('s')
                    .filter(|s| RELATIONS.contains(s))
                    .unwrap_or(other),
            };
            RELATIONS.contains(&singular).then(|| singular.to_owned())
        })
}

/// The same relation, however it was said: `mom` and `mother` are one.
fn same_relation(a: &str, b: &str) -> bool {
    let canonical = |r: &str| {
        match r {
            "mum" | "mom" => "mother",
            "dad" => "father",
            "kid" => "child",
            "grandkid" => "grandchild",
            "neighbour" => "neighbor",
            "carer" => "caregiver",
            other => other,
        }
        .to_owned()
    };
    canonical(a) == canonical(b)
}

fn name_matches(name: &str, query: &str) -> bool {
    let name = name.to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| name.contains(word))
}

/// What a search for someone found.
pub(crate) enum Found {
    /// These people, with who they are to the user if known.
    People(Vec<(Contact, Option<String>)>),
    /// The user named a relation nobody is remembered as.
    UnknownRelation(String),
}

/// Resolves people, and learns who they are.
pub(crate) struct People {
    contacts: Option<Arc<dyn Contacts>>,
    memory: Option<Arc<dyn PeopleMemory>>,
    /// A relation the user mentioned that is not remembered yet: "son".
    unknown: Mutex<Option<String>>,
}

impl People {
    pub(crate) fn new(
        contacts: Option<Arc<dyn Contacts>>,
        memory: Option<Arc<dyn PeopleMemory>>,
    ) -> Self {
        Self {
            contacts,
            memory,
            unknown: Mutex::new(None),
        }
    }

    pub(crate) fn available(&self) -> bool {
        self.contacts.is_some() || self.memory.is_some()
    }

    /// The relation the user mentioned but nobody is remembered as.
    pub(crate) fn unknown_relation(&self) -> Option<String> {
        self.unknown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_unknown(&self, relation: Option<String>) {
        *self.unknown.lock().unwrap_or_else(PoisonError::into_inner) = relation;
    }

    /// Who `who` may be: a relation ("my son") or a name.
    pub(crate) async fn find(&self, who: &str, cx: &Ctx) -> Result<Found, String> {
        let remembered = match &self.memory {
            Some(memory) => memory.people(cx).await.map_err(|e| explain(&e, "Memory"))?,
            None => Vec::new(),
        };
        if let Some(relation) = relation(who) {
            let known: Vec<(Contact, Option<String>)> = remembered
                .into_iter()
                .filter(|p| same_relation(&p.relation, &relation))
                .map(|p| (p.contact, Some(p.relation)))
                .collect();
            if known.is_empty() {
                self.set_unknown(Some(relation.clone()));
                return Ok(Found::UnknownRelation(relation));
            }
            return Ok(Found::People(known));
        }
        let mut found: Vec<(Contact, Option<String>)> = remembered
            .into_iter()
            .filter(|p| name_matches(&p.contact.name, who))
            .map(|p| (p.contact, Some(p.relation)))
            .collect();
        if let Some(contacts) = &self.contacts {
            let results = contacts
                .search(who, cx)
                .await
                .map_err(|e| explain(&e, "The address book"))?;
            for contact in results {
                let same = |c: &Contact| {
                    c.id == contact.id || c.emails.iter().any(|e| contact.emails.contains(e))
                };
                if !found.iter().any(|(c, _)| same(c)) {
                    found.push((contact, None));
                }
            }
        }
        Ok(Found::People(found))
    }

    /// Remembers who `contact` is to the user.
    pub(crate) async fn learn(
        &self,
        relation: &str,
        contact: Contact,
        cx: &Ctx,
    ) -> Result<(), ServiceError> {
        let Some(memory) = &self.memory else {
            return Err(ServiceError::NotSetUp("no memory".into()));
        };
        memory
            .remember(
                KnownPerson {
                    relation: relation.to_owned(),
                    contact,
                },
                cx,
            )
            .await?;
        if self
            .unknown_relation()
            .is_some_and(|r| same_relation(&r, relation))
        {
            self.set_unknown(None);
        }
        Ok(())
    }
}

/// Finding and remembering people.
pub(crate) struct PeopleTools(Arc<Shared>);

pub(crate) fn offer(shared: &Arc<Shared>) -> Option<Arc<dyn Toolbox>> {
    shared
        .people
        .available()
        .then(|| Arc::new(PeopleTools(Arc::clone(shared))) as Arc<dyn Toolbox>)
}

#[derive(Deserialize)]
struct WhoArgs {
    #[serde(alias = "name", alias = "query")]
    who: String,
}

#[derive(Deserialize)]
struct RememberArgs {
    relation: String,
    #[serde(alias = "contact", alias = "id")]
    person: String,
}

fn line(handle: &str, contact: &Contact, relation: Option<&str>) -> String {
    let mut out = format!("{handle}: {}", contact.name);
    if let Some(relation) = relation {
        let _ = write!(out, ", the user's {relation}");
    }
    match contact.emails.split_first() {
        Some((first, [])) => {
            let _ = write!(out, ", {first}");
        }
        Some((first, rest)) => {
            let _ = write!(out, ", {first} (and {} other addresses)", rest.len());
        }
        None => out.push_str(", no email address"),
    }
    out
}

impl PeopleTools {
    async fn find(&self, who: &str, cx: &Ctx) -> ToolOutput {
        let who = who.trim();
        if who.is_empty() {
            return ToolOutput::failed("Say who to look for.");
        }
        match self.0.people.find(who, cx).await {
            Err(problem) => ToolOutput::failed(problem),
            Ok(Found::UnknownRelation(relation)) => ToolOutput::ok(format!(
                "Nobody is saved as the user's {relation} yet. Ask for their name, then call \
                 find_person with the name."
            )),
            Ok(Found::People(found)) if found.is_empty() => ToolOutput::ok(format!(
                "No one in the address book matches \"{who}\". Ask the user to say the name \
                 again, or to spell it."
            )),
            Ok(Found::People(found)) => {
                let mut out = String::new();
                for (contact, relation) in found.iter().take(MAX_PEOPLE) {
                    let handle = self.0.items.add(Item::Person {
                        contact: contact.clone(),
                        relation: relation.clone(),
                    });
                    let _ = writeln!(out, "{}", line(&handle, contact, relation.as_deref()));
                }
                more(&mut out, found.len(), MAX_PEOPLE);
                if found.len() > 1 {
                    out.push_str("More than one match: ask the user which one, by name.");
                }
                ToolOutput::ok(out.trim_end().to_owned())
            }
        }
    }

    fn person(&self, handle: &str) -> Result<Contact, String> {
        match self.0.items.get(handle) {
            Some(Item::Person { contact, .. }) => Ok(contact),
            _ => Err(format!(
                "Unknown person `{handle}`: use an id like c1 from find_person."
            )),
        }
    }
}

#[async_trait]
impl Toolbox for PeopleTools {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![spec(
            FIND_PERSON,
            "Find someone by name or by who they are to the user, e.g. \"David\" or \"my son\". \
             Returns ids like c1 that other tools take.",
            r#"{"type":"object","properties":{"who":{"type":"string","description":"A name, or the user's words for them, e.g. my son"}},"required":["who"]}"#,
        )];
        if self.0.caps.memory.is_some() {
            specs.push(spec(
                REMEMBER_PERSON,
                "Remember who someone is to the user, once the user has confirmed it, e.g. that \
                 c1 is their son.",
                r#"{"type":"object","properties":{"relation":{"type":"string","description":"e.g. son, daughter, doctor"},"person":{"type":"string","description":"An id from find_person, e.g. c1"}},"required":["relation","person"]}"#,
            ));
        }
        specs
    }

    async fn assess(&self, call: &ToolCall, _cx: &Ctx) -> Result<Assessment, String> {
        match call.name.as_str() {
            FIND_PERSON => {
                let WhoArgs { who } = args(call)?;
                Ok(Assessment {
                    summary: format!("look up {}", who.trim()),
                    consequence: Consequence::Observe,
                    progress: Some(format!("Looking up {}", who.trim())),
                })
            }
            REMEMBER_PERSON => {
                let RememberArgs { relation, person } = args(call)?;
                let contact = self.person(&person)?;
                Ok(Assessment {
                    summary: format!("remember that {} is your {}", contact.name, relation.trim()),
                    consequence: Consequence::Reversible,
                    progress: None,
                })
            }
            other => Err(format!("There is no tool named `{other}`.")),
        }
    }

    async fn run(&self, call: &ToolCall, cx: &Ctx) -> ToolOutput {
        match call.name.as_str() {
            FIND_PERSON => match args::<WhoArgs>(call) {
                Ok(WhoArgs { who }) => self.find(&who, cx).await,
                Err(problem) => ToolOutput::failed(problem),
            },
            REMEMBER_PERSON => {
                let (said, handle) = match args::<RememberArgs>(call) {
                    Ok(a) => (a.relation, a.person),
                    Err(problem) => return ToolOutput::failed(problem),
                };
                let contact = match self.person(&handle) {
                    Ok(c) => c,
                    Err(problem) => return ToolOutput::failed(problem),
                };
                let relation = relation(&said).unwrap_or_else(|| said.trim().to_lowercase());
                let name = contact.name.clone();
                match self.0.people.learn(&relation, contact, cx).await {
                    Ok(()) => {
                        self.0.items.set_relation(&handle, &relation);
                        ToolOutput::ok(format!("Remembered that {name} is the user's {relation}."))
                    }
                    Err(error) => ToolOutput::failed(explain(&error, "Memory")),
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
    fn relations_are_recognized_however_they_are_said() {
        assert_eq!(relation("my son").as_deref(), Some("son"));
        assert_eq!(relation("our grandkids").as_deref(), Some("grandkid"));
        assert_eq!(relation("the grandchildren").as_deref(), Some("grandchild"));
        assert_eq!(relation("Dr. Patel, my doctor").as_deref(), Some("doctor"));
        assert_eq!(relation("David"), None);
        assert_eq!(
            relation("Chris"),
            None,
            "a name ending in s is not a plural"
        );
        assert!(same_relation("mom", "mother"));
        assert!(!same_relation("son", "daughter"));
    }
}
