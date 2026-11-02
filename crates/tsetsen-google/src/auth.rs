//! Signing in with Google, and keeping an access token fresh.
//!
//! Sign-in is the installed-app flow: the browser opens Google's consent page,
//! Google redirects to a one-shot server on `127.0.0.1`, and the code is
//! exchanged with PKCE, so an intercepted code is useless. Only the refresh
//! token is stored, in the OS keychain. Access tokens live in memory.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tsetsen_core::{Ctx, ServiceError};
use url::Url;

use crate::http::{Endpoints, send};

/// What Tsetsen asks Google for: sending mail, and reading contacts to find
/// who to send it to. Nothing that reads mail.
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/gmail.send",
    "https://www.googleapis.com/auth/contacts.readonly",
    "https://www.googleapis.com/auth/contacts.other.readonly",
];

/// How long sign-in waits for the user to finish in the browser.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// Tokens this close to expiry are refreshed first.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// A secret string. Never printed, never logged.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps `value`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for the one place that sends it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// An OAuth client of the "Desktop app" type, from the Google Cloud console.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthClient {
    /// `….apps.googleusercontent.com`.
    pub client_id: String,
    /// For desktop clients Google treats this as public, but it is still kept
    /// out of logs and config files.
    pub client_secret: Secret,
}

impl OAuthClient {
    /// Reads the JSON file the console downloads (`{"installed": {…}}`).
    pub fn from_console_json(json: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct File {
            installed: OAuthClient,
        }
        serde_json::from_str::<File>(json)
            .map(|f| f.installed)
            .map_err(|e| format!("not a Google OAuth client file for a desktop app ({e})"))
    }
}

/// What is kept between runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// The OAuth client.
    pub client: OAuthClient,
    /// Lets Tsetsen get access tokens without asking again.
    pub refresh_token: Secret,
}

/// Where credentials are kept.
pub trait CredentialStore: Send + Sync {
    /// The saved credentials, if signed in.
    fn load(&self) -> Result<Option<Credentials>, ServiceError>;
    /// Saves `credentials`, replacing any before.
    fn save(&self, credentials: &Credentials) -> Result<(), ServiceError>;
    /// Forgets them.
    fn clear(&self) -> Result<(), ServiceError>;
}

/// The OS keychain: macOS Keychain, Windows Credential Manager, or the
/// Secret Service on Linux.
#[derive(Debug, Clone)]
pub struct Keychain {
    service: String,
    account: String,
}

impl Keychain {
    /// The item for Tsetsen's Google sign-in.
    #[must_use]
    pub fn google() -> Self {
        Self {
            service: "ai.tsetsen.google".into(),
            account: "default".into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, ServiceError> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| ServiceError::Unavailable(format!("the keychain is unavailable: {e}")))
    }
}

impl CredentialStore for Keychain {
    fn load(&self) -> Result<Option<Credentials>, ServiceError> {
        match self.entry()?.get_password() {
            Ok(json) => serde_json::from_str(&json).map(Some).map_err(|_| {
                ServiceError::NotSetUp(
                    "the saved Google sign-in is unreadable; sign in again".into(),
                )
            }),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(ServiceError::Unavailable(format!(
                "the keychain failed: {e}"
            ))),
        }
    }

    fn save(&self, credentials: &Credentials) -> Result<(), ServiceError> {
        let json = serde_json::to_string(credentials)
            .map_err(|e| ServiceError::Rejected(e.to_string()))?;
        self.entry()?
            .set_password(&json)
            .map_err(|e| ServiceError::Unavailable(format!("the keychain failed: {e}")))
    }

    fn clear(&self) -> Result<(), ServiceError> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(ServiceError::Unavailable(format!(
                "the keychain failed: {e}"
            ))),
        }
    }
}

/// Credentials in memory, for tests.
#[derive(Debug, Default)]
pub struct InMemoryStore(std::sync::Mutex<Option<Credentials>>);

impl CredentialStore for InMemoryStore {
    fn load(&self) -> Result<Option<Credentials>, ServiceError> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn save(&self, credentials: &Credentials) -> Result<(), ServiceError> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(credentials.clone());
        Ok(())
    }

    fn clear(&self) -> Result<(), ServiceError> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Secret,
    #[serde(default)]
    expires_in: u64,
    refresh_token: Option<Secret>,
}

fn random_token() -> Result<String, ServiceError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| ServiceError::Unavailable(format!("no randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Signs in through the browser and saves the result in `store`.
/// `open_browser` is given the consent page's URL.
pub async fn sign_in(
    client: OAuthClient,
    store: &dyn CredentialStore,
    endpoints: &Endpoints,
    open_browser: impl FnOnce(&str),
) -> Result<(), ServiceError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| ServiceError::Unavailable(format!("cannot listen for the sign-in: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| ServiceError::Unavailable(e.to_string()))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = random_token()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token()?;
    let mut consent =
        Url::parse(&endpoints.auth).map_err(|e| ServiceError::Rejected(e.to_string()))?;
    consent
        .query_pairs_mut()
        .append_pair("client_id", &client.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    open_browser(consent.as_str());

    let code = tokio::time::timeout(SIGN_IN_TIMEOUT, receive_code(&listener, &state))
        .await
        .map_err(|_| ServiceError::Unavailable("sign-in was not finished in time".into()))??;

    let form = [
        ("code", code.as_str()),
        ("client_id", client.client_id.as_str()),
        ("client_secret", client.client_secret.expose()),
        ("redirect_uri", redirect.as_str()),
        ("grant_type", "authorization_code"),
        ("code_verifier", verifier.as_str()),
    ];
    let http = reqwest::Client::new();
    let response: TokenResponse = token_request(&http, endpoints, &form).await?;
    let refresh_token = response.refresh_token.ok_or_else(|| {
        ServiceError::Rejected(
            "Google gave no refresh token; remove Tsetsen's access and retry".into(),
        )
    })?;
    store.save(&Credentials {
        client,
        refresh_token,
    })
}

/// Waits for Google's redirect and returns the code, ignoring stray requests
/// (a browser asks for `/favicon.ico` too).
async fn receive_code(listener: &TcpListener, state: &str) -> Result<String, ServiceError> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|e| ServiceError::Unavailable(e.to_string()))?;
        let mut buffer = vec![0u8; 8192];
        let n = socket.read(&mut buffer).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..n]);
        let target = request.split_whitespace().nth(1).unwrap_or("/");
        let Ok(url) = Url::parse(&format!("http://localhost{target}")) else {
            continue;
        };
        let get = |key: &str| {
            url.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };
        let (reply, result) = match (get("state"), get("code"), get("error")) {
            (Some(s), Some(code), _) if s == state => {
                ("You're signed in. You can close this tab.", Some(Ok(code)))
            }
            (Some(s), _, Some(error)) if s == state => (
                "Sign-in was cancelled. You can close this tab.",
                Some(Err(ServiceError::PermissionDenied(format!(
                    "sign-in failed: {error}"
                )))),
            ),
            _ => ("Not found.", None),
        };
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>Tsetsen</title>\
             <body style=\"font:24px system-ui;margin:4em\">{reply}</body>"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        if let Some(result) = result {
            return result;
        }
    }
}

async fn token_request(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    form: &[(&str, &str)],
) -> Result<TokenResponse, ServiceError> {
    let response = http
        .post(&endpoints.token)
        .form(form)
        .send()
        .await
        .map_err(|e| ServiceError::Unavailable(format!("Google sign-in is unreachable: {e}")))?;
    let status = response.status();
    if status.is_success() {
        return response
            .json()
            .await
            .map_err(|e| ServiceError::Unavailable(format!("bad token response: {e}")));
    }
    let body = response.text().await.unwrap_or_default();
    Err(if body.contains("invalid_grant") {
        ServiceError::NotSetUp(
            "the Google sign-in has expired or was revoked; sign in again".into(),
        )
    } else if status.is_server_error() {
        ServiceError::Unavailable(format!("Google sign-in failed ({status})"))
    } else {
        ServiceError::Rejected(format!("Google sign-in refused ({status})"))
    })
}

/// Hands out access tokens, refreshing them as needed.
pub struct Auth {
    http: reqwest::Client,
    endpoints: Endpoints,
    store: Arc<dyn CredentialStore>,
    cached: Mutex<Option<(Secret, Instant)>>,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// Tokens for the account saved in `store`.
    #[must_use]
    pub fn new(
        http: reqwest::Client,
        endpoints: Endpoints,
        store: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            http,
            endpoints,
            store,
            cached: Mutex::new(None),
        }
    }

    /// Tokens from Google itself, for the account saved in `store`.
    pub fn google(store: Arc<dyn CredentialStore>) -> Result<Self, ServiceError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| ServiceError::Unavailable(format!("no HTTP client: {e}")))?;
        Ok(Self::new(http, Endpoints::default(), store))
    }

    /// The endpoints in use.
    #[must_use]
    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// The HTTP client in use.
    #[must_use]
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Whether someone has signed in.
    pub fn signed_in(&self) -> Result<bool, ServiceError> {
        Ok(self.store.load()?.is_some())
    }

    /// A current access token.
    pub async fn token(&self, cx: &Ctx) -> Result<Secret, ServiceError> {
        let mut cached = self.cached.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && Instant::now() + EXPIRY_MARGIN < *expires
        {
            return Ok(token.clone());
        }
        let credentials = self.store.load()?.ok_or_else(|| {
            ServiceError::NotSetUp(
                "nobody has signed in to Google; run `tsetsen google login`".into(),
            )
        })?;
        let form = [
            ("client_id", credentials.client.client_id.as_str()),
            ("client_secret", credentials.client.client_secret.expose()),
            ("refresh_token", credentials.refresh_token.expose()),
            ("grant_type", "refresh_token"),
        ];
        let response = send(cx, token_request(&self.http, &self.endpoints, &form)).await??;
        let lifetime = Duration::from_secs(response.expires_in.max(60));
        *cached = Some((response.access_token.clone(), Instant::now() + lifetime));
        Ok(response.access_token)
    }

    /// Drops the cached token, after Google rejected it.
    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_console_file_and_never_prints_secrets() {
        let client = OAuthClient::from_console_json(
            r#"{"installed":{"client_id":"123.apps.googleusercontent.com","project_id":"p",
                "auth_uri":"https://accounts.google.com/o/oauth2/auth","client_secret":"GOCSPX-shh",
                "redirect_uris":["http://localhost"]}}"#,
        )
        .unwrap();
        assert_eq!(client.client_id, "123.apps.googleusercontent.com");
        assert!(!format!("{client:?}").contains("shh"));
        assert!(OAuthClient::from_console_json(r#"{"web":{}}"#).is_err());
    }
}
