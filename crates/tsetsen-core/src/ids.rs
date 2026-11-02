//! Strongly typed identifiers, so a window id can never be passed as a provider id.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates the identifier from any string-like value.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// The identifier as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

string_id!(
    /// A configured model provider, e.g. `parakeet-v3`.
    ProviderId
);
string_id!(
    /// A platform window, stable for the window's lifetime.
    WindowId
);
string_id!(
    /// A UI element inside a window, as reported by the desktop adapter.
    ElementRef
);
string_id!(
    /// An application: bundle id on macOS, AUMID or executable on Windows,
    /// desktop-file id on Linux.
    AppId
);
string_id!(
    /// A BCP 47 language tag, e.g. `en`, `en-US`, `mn`.
    LanguageTag
);
string_id!(
    /// The key of one question within a decision request.
    QuestionKey
);
string_id!(
    /// A voice a text-to-speech provider offers, e.g. `F1`.
    VoiceId
);

impl LanguageTag {
    /// The primary language subtag: `en` for `en-US`.
    #[must_use]
    pub fn primary(&self) -> &str {
        self.0.split(['-', '_']).next().unwrap_or_default()
    }

    /// Whether both tags name the same primary language (`en-US` matches `en`).
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.primary().eq_ignore_ascii_case(other.primary())
    }
}

/// One user request, end to end: logs, journal entries and traces share it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(u64);

impl RequestId {
    /// Wraps a raw id.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "req-{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::LanguageTag;

    #[test]
    fn language_tags_match_on_primary_subtag() {
        let en = LanguageTag::from("en");
        assert!(en.matches(&LanguageTag::from("en-US")));
        assert!(en.matches(&LanguageTag::from("EN_gb")));
        assert!(!en.matches(&LanguageTag::from("mn")));
    }
}
