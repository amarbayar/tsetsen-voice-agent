//! Domain types and port traits for Tsetsen.
//!
//! This crate is the center of the hexagon. It defines what the system does and
//! the interfaces ("ports") that adapters implement. It has no I/O, no async
//! runtime, no OS APIs and no vendor SDKs, so every dependency in the workspace
//! points inward to it and everything here can be tested in isolation.

pub mod ctx;
pub mod error;
pub mod ids;
pub mod platform;
pub mod ports;
pub mod provider;
pub mod sensitivity;

pub use ctx::{Cancellation, Ctx, Interrupted};
pub use error::{AudioError, DesktopError, ProviderError, ServiceError};
pub use ids::{
    AppId, ElementRef, LanguageTag, ProviderId, QuestionKey, RequestId, VoiceId, WindowId,
};
pub use platform::Os;
pub use provider::{Describe, Layer, Locality, ProviderDescriptor};
pub use sensitivity::Sensitivity;
