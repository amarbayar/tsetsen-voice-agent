//! What every model provider declares about itself.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{LanguageTag, ProviderId};

/// The pipeline layer a provider serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Speech to text.
    Stt,
    /// Transcript cleanup and self-correction.
    Rewrite,
    /// Typed decisions: routing, element choice, risk.
    Decide,
    /// Multi-step planning.
    Plan,
    /// Conversational replies: the thinker in thinker-talker.
    Think,
    /// Text to speech: the talker.
    Speak,
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stt => "stt",
            Self::Rewrite => "rewrite",
            Self::Decide => "decide",
            Self::Plan => "plan",
            Self::Think => "think",
            Self::Speak => "speak",
        })
    }
}

/// Where a provider runs. Policy reasons about this, never about vendor names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Locality {
    /// On this device, in-process or in a local worker.
    Local,
    /// On another machine.
    Remote {
        /// The host requests go to.
        host: String,
    },
}

impl Locality {
    /// Whether data sent to this provider leaves the device.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }
}

/// Static facts about a provider, declared by its adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProviderDescriptor {
    /// Configured id, unique across the registry.
    pub id: ProviderId,
    /// The layer it serves.
    pub layer: Layer,
    /// Where it runs.
    pub locality: Locality,
    /// Supported languages; `None` means any.
    pub languages: Option<Vec<LanguageTag>>,
}

impl ProviderDescriptor {
    /// A descriptor that accepts any language.
    #[must_use]
    pub fn new(id: impl Into<ProviderId>, layer: Layer, locality: Locality) -> Self {
        Self {
            id: id.into(),
            layer,
            locality,
            languages: None,
        }
    }

    /// Restricts the provider to `languages`.
    #[must_use]
    pub fn with_languages(mut self, languages: impl IntoIterator<Item = LanguageTag>) -> Self {
        self.languages = Some(languages.into_iter().collect());
        self
    }

    /// Whether the provider handles `language`.
    #[must_use]
    pub fn supports_language(&self, language: &LanguageTag) -> bool {
        self.languages
            .as_ref()
            .is_none_or(|supported| supported.iter().any(|tag| tag.matches(language)))
    }
}

/// Implemented by every model provider so routing and policy can inspect it.
pub trait Describe {
    /// The provider's static description.
    fn descriptor(&self) -> &ProviderDescriptor;
}
