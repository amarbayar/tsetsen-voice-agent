//! Google services for Tsetsen: Gmail for sending, Google Contacts for
//! finding who to send to.
//!
//! The user signs in once in their browser ([`sign_in`]); the refresh token
//! is kept in the OS keychain, never in a file. Access is limited to sending
//! mail and reading contacts: Tsetsen cannot read the user's mail.

mod auth;
mod gmail;
mod http;
mod mime;
mod outbox;
mod people;

pub use auth::{
    Auth, CredentialStore, Credentials, InMemoryStore, Keychain, OAuthClient, SCOPES, Secret,
    sign_in,
};
pub use gmail::GmailMailer;
pub use http::Endpoints;
pub use outbox::OutboxMailer;
pub use people::GoogleContacts;
