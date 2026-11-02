//! Which operating system this is. Adapters differ per OS; the tools above
//! them do not.

/// An operating system family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Os {
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
    /// Linux and other freedesktop systems.
    Linux,
    /// Anything else.
    Other,
}

impl Os {
    /// The OS this was built for.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }

    /// How people call it: "macOS".
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::MacOs => "macOS",
            Self::Windows => "Windows",
            Self::Linux => "Linux",
            Self::Other => "this system",
        }
    }
}
