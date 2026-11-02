//! Ports: the interfaces adapters implement.
//!
//! Model ports are split by capability rather than merged into one "model"
//! trait. Speech-to-text, cleanup and typed decisions have different inputs and
//! outputs, and a provider implements only the capabilities it really has.

mod contacts;
mod decide;
mod desktop;
mod files;
mod mail;
mod photos;
mod playback;
mod rewrite;
mod speak;
mod stt;
mod think;
mod vad;

pub use contacts::{Contact, Contacts, KnownPerson, PeopleMemory};
pub use decide::{
    Answer, ChoiceOption, Decider, DecisionState, Distribution, InvalidDistribution, Question,
};
pub use desktop::{
    ActionReport, Anchor, Delivery, Desktop, TextEntry, UiAction, UiNode, WindowInfo,
    WindowSnapshot,
};
pub use files::{FileInfo, FileQuery, FileSearch, Launcher};
pub use mail::{Mailbox, Mailer, OutgoingMail, SentMail};
pub use photos::{ExportedPhoto, GeoPoint, PhotoInfo, PhotoLibrary, PhotoQuery, Showcase};
pub use playback::{Playback, Played};
pub use rewrite::{Rewrite, RewriteRequest, Rewriter};
pub use speak::{SpeechRequest, TextToSpeech};
pub use stt::{AudioClip, InvalidAudio, SpeechToText, SttHints, Transcript};
pub use think::{ThinkEvent, ThinkRequest, ThinkStream, Thinker, ToolCall, ToolSpec, Turn};
pub use vad::VoiceActivity;
