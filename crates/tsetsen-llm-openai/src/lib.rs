//! OpenAI-compatible chat-completions adapter.
//!
//! One client covers every server that speaks `POST /v1/chat/completions`:
//! Ollama, LM Studio, `mlx_lm.server`, vLLM, llama.cpp's `llama-server`,
//! `OpenRouter` and most hosted APIs. It implements [`Rewriter`] and the
//! streaming [`Thinker`]; a `Planner` will reuse the same client.
//!
//! The provider's [`Locality`] is derived from the base URL, never declared by
//! configuration, so a remote endpoint cannot be passed off as local to the
//! egress policy.
//!
//! [`Rewriter`]: tsetsen_core::ports::Rewriter
//! [`Thinker`]: tsetsen_core::ports::Thinker
//! [`Locality`]: tsetsen_core::Locality

mod client;
mod config;
mod prompt;
mod rewriter;
mod sse;
mod thinker;

pub use config::{ApiKey, ConfigError, OpenAiConfig};
pub use rewriter::OpenAiRewriter;
pub use thinker::OpenAiThinker;
