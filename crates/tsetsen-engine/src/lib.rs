//! Tsetsen pipelines, composed purely from ports.
//!
//! Nothing in this crate knows which OS, model runtime or vendor it runs
//! against. Adapters are chosen in the composition root (the daemon) and
//! handed in as trait objects.

mod cancel;
mod claims;
mod conversation;
mod desk;
mod desktop_tools;
mod dictation;
mod errands;
mod guard;
mod session;
mod speakable;
mod stt;
mod tools;
mod turn;
pub mod when;

pub use conversation::{
    ActionOutcome, ActionRecord, Conversation, ConversationConfig, ConversationEngine,
    ConversationError, ConversationEvent, Observer, Pacing, Reply, ReplyTiming, TurnOutcome,
    Unobserved, VOICE_INSTRUCTIONS,
};
pub use desk::{DESK_INSTRUCTIONS, DeskConfig, WORKER_INSTRUCTIONS};
pub use desktop_tools::DesktopTools;
pub use dictation::{
    DictationConfig, DictationEngine, DictationError, DictationOutcome, DictationRequest,
    HoldReason, Provenance, RewriteBudget, RewriteStatus,
};
pub use errands::{Capabilities, Clock, MAX_SHOWN, catalog, spoken_address};
pub use guard::{OverlapGuard, RewriteCheck, RewriteGuard};
pub use session::{SessionConfig, SessionError, TurnResult, run_session};
pub use speakable::{ClauseChunker, speakable};
pub use stt::TranscribeError;
pub use tools::{Assessment, DuplicateTool, ToolOutput, ToolSet, Toolbox};
pub use turn::{EndpointConfig, Endpointer, TurnEvent};
