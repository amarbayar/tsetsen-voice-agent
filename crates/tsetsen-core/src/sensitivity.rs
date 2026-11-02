//! Data sensitivity labels.

use serde::{Deserialize, Serialize};

/// How sensitive a piece of data is, ordered from least to most sensitive.
///
/// The ordering is what egress policy compares against: a provider granted
/// [`Sensitivity::Personal`] may receive `Public` and `Personal` data and
/// nothing above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// Safe to share with anyone.
    Public,
    /// About the user: transcripts, app names, preferences.
    Personal,
    /// Read from the screen: window titles, field values, documents.
    ScreenContent,
    /// Raw microphone audio. A voice is biometric data.
    Audio,
    /// Credentials and anything read from secure fields. Never leaves the device.
    Secret,
}

impl Sensitivity {
    /// Every label, least sensitive first.
    pub const ALL: [Self; 5] = [
        Self::Public,
        Self::Personal,
        Self::ScreenContent,
        Self::Audio,
        Self::Secret,
    ];
}
