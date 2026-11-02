use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use tsetsen_core::{LanguageTag, Locality, ProviderId};
use url::{Host, Url};

/// A bearer token. Its `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wraps a key. Load it from the OS keychain, never from a config file or argv.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// The endpoint configuration is unusable.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The base URL does not parse.
    #[error("invalid base URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    /// The base URL is neither `http` nor `https`, or has no host.
    #[error("base URL must be http(s) with a host, got `{0}`")]
    UnsupportedUrl(String),
    /// Plain HTTP is only allowed to this machine. Anything else would send
    /// transcripts, and the API key, over the network unencrypted.
    #[error("refusing plain HTTP to remote host `{0}`; use https")]
    InsecureRemote(String),
    /// URLs with embedded credentials are refused; use [`ApiKey`].
    #[error("base URL must not contain credentials")]
    CredentialsInUrl,
    /// The HTTP client could not be built.
    #[error("HTTP client: {0}")]
    Http(#[from] reqwest::Error),
}

/// How to reach one OpenAI-compatible endpoint.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub(crate) id: ProviderId,
    pub(crate) base_url: Url,
    pub(crate) locality: Locality,
    pub(crate) model: String,
    pub(crate) api_key: Option<ApiKey>,
    pub(crate) request_timeout: Duration,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) languages: Option<Vec<LanguageTag>>,
}

impl OpenAiConfig {
    /// Default per-request timeout. The engine usually imposes a tighter one.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// An endpoint at `base_url` (e.g. `http://localhost:11434/v1`) serving `model`.
    pub fn new(
        id: impl Into<ProviderId>,
        base_url: &str,
        model: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let mut base_url = Url::parse(base_url)?;
        if !base_url.username().is_empty() || base_url.password().is_some() {
            return Err(ConfigError::CredentialsInUrl);
        }
        let locality = locality_of(&base_url)?;
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        Ok(Self {
            id: id.into(),
            base_url,
            locality,
            model: model.into(),
            api_key: None,
            request_timeout: Self::DEFAULT_TIMEOUT,
            reasoning_effort: None,
            languages: None,
        })
    }

    /// Sends `key` as a bearer token.
    #[must_use]
    pub fn with_api_key(mut self, key: ApiKey) -> Self {
        self.api_key = Some(key);
        self
    }

    /// Caps each request at `timeout`.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Sends `reasoning_effort`. Use `"none"` for hybrid thinking models
    /// (Qwen3.5 on Ollama): left on, they spend the whole token budget
    /// reasoning and return no text. Leave unset for servers that reject the
    /// parameter.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// Restricts the provider to `languages`, so the router skips it for others.
    #[must_use]
    pub fn with_languages(mut self, languages: impl IntoIterator<Item = LanguageTag>) -> Self {
        self.languages = Some(languages.into_iter().collect());
        self
    }

    /// Where requests go, as derived from the base URL.
    #[must_use]
    pub fn locality(&self) -> &Locality {
        &self.locality
    }

    pub(crate) fn endpoint(&self, path: &str) -> Result<Url, url::ParseError> {
        self.base_url.join(path)
    }
}

/// Loopback hosts are local. Everything else, LAN hosts included, is remote:
/// data sent there leaves this device.
fn locality_of(url: &Url) -> Result<Locality, ConfigError> {
    let loopback = match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => return Err(ConfigError::UnsupportedUrl(url.to_string())),
    };
    let host = || url.host_str().unwrap_or_default().to_owned();
    match (url.scheme(), loopback) {
        ("http" | "https", true) => Ok(Locality::Local),
        ("https", false) => Ok(Locality::Remote { host: host() }),
        ("http", false) => Err(ConfigError::InsecureRemote(host())),
        _ => Err(ConfigError::UnsupportedUrl(url.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locality(url: &str) -> Result<Locality, ConfigError> {
        OpenAiConfig::new("p", url, "m").map(|c| c.locality)
    }

    #[test]
    fn loopback_endpoints_are_local() {
        for url in [
            "http://localhost:11434/v1",
            "http://LOCALHOST:1234/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:8080/v1",
        ] {
            assert_eq!(locality(url).unwrap(), Locality::Local, "{url}");
        }
    }

    #[test]
    fn other_hosts_are_remote_even_on_the_lan() {
        assert_eq!(
            locality("https://192.168.1.20/v1").unwrap(),
            Locality::Remote {
                host: "192.168.1.20".into()
            }
        );
        assert_eq!(
            locality("https://openrouter.ai/api/v1").unwrap(),
            Locality::Remote {
                host: "openrouter.ai".into()
            }
        );
    }

    #[test]
    fn plain_http_to_remote_hosts_is_refused() {
        assert!(matches!(
            locality("http://example.com/v1"),
            Err(ConfigError::InsecureRemote(_))
        ));
    }

    #[test]
    fn odd_urls_are_refused() {
        assert!(matches!(
            locality("ftp://localhost/v1"),
            Err(ConfigError::UnsupportedUrl(_))
        ));
        assert!(matches!(
            locality("https://user:pw@example.com/v1"),
            Err(ConfigError::CredentialsInUrl)
        ));
        assert!(matches!(
            locality("not a url"),
            Err(ConfigError::InvalidUrl(_))
        ));
    }

    #[test]
    fn endpoint_paths_join_under_the_base_path() {
        let config = OpenAiConfig::new("p", "https://openrouter.ai/api/v1", "m").unwrap();
        assert_eq!(
            config.endpoint("chat/completions").unwrap().as_str(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
    }

    #[test]
    fn debug_output_never_shows_the_key() {
        let config = OpenAiConfig::new("p", "https://example.com/v1", "m")
            .unwrap()
            .with_api_key(ApiKey::new("sk-very-secret"));
        assert!(!format!("{config:?}").contains("sk-very-secret"));
    }
}
