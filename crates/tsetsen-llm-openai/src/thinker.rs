use std::fmt::Write as _;

use async_trait::async_trait;
use tsetsen_core::ports::{ThinkRequest, ThinkStream, Thinker, Turn};
use tsetsen_core::{Ctx, Describe, Layer, ProviderDescriptor, ProviderError};

use crate::client::{ChatClient, Message, Role, Sampling, WireFunction, WireToolCall};
use crate::config::{ConfigError, OpenAiConfig};

/// Conversational replies, streamed, through any OpenAI-compatible endpoint.
pub struct OpenAiThinker {
    descriptor: ProviderDescriptor,
    client: ChatClient,
    sampling: Sampling,
}

impl OpenAiThinker {
    /// Builds a thinker for the endpoint in `config`. Replies are capped at
    /// 400 tokens and sampled at temperature 0.6 unless changed.
    pub fn new(config: &OpenAiConfig) -> Result<Self, ConfigError> {
        let mut descriptor =
            ProviderDescriptor::new(config.id.clone(), Layer::Think, config.locality.clone());
        if let Some(languages) = &config.languages {
            descriptor = descriptor.with_languages(languages.iter().cloned());
        }
        Ok(Self {
            descriptor,
            client: ChatClient::new(config)?,
            sampling: Sampling {
                temperature: 0.6,
                max_tokens: 400,
            },
        })
    }

    /// Sampling temperature; 0 is deterministic. Clamped to `0.0..=2.0`.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.sampling.temperature = if temperature.is_finite() {
            temperature.clamp(0.0, 2.0)
        } else {
            0.0
        };
        self
    }

    /// The longest reply, in tokens. At least 16.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.sampling.max_tokens = max_tokens.max(16);
        self
    }
}

impl Describe for OpenAiThinker {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
}

fn messages(request: &ThinkRequest) -> Vec<Message> {
    let mut messages = Vec::with_capacity(request.turns.len() + 1);
    let mut system = request.instructions.clone();
    if let Some(language) = &request.language {
        // Writing to a String cannot fail.
        let _ = write!(system, "\nReply in the language with tag `{language}`.");
    }
    if !system.trim().is_empty() {
        messages.push(Message::new(Role::System, system));
    }
    messages.extend(request.turns.iter().map(|turn| {
        match turn {
            Turn::User { text } => Message::new(Role::User, text.clone()),
            Turn::Assistant { text, tool_calls } => Message {
                tool_calls: tool_calls
                    .iter()
                    .map(|call| WireToolCall {
                        id: call.id.clone(),
                        kind: "function",
                        function: WireFunction {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        },
                    })
                    .collect(),
                ..Message::new(Role::Assistant, text.clone())
            },
            Turn::ToolResult { call_id, content } => Message {
                tool_call_id: Some(call_id.clone()),
                ..Message::new(Role::Tool, content.clone())
            },
            // Mid-conversation system messages are not portable across chat
            // templates; a labelled user message is.
            Turn::Event { from, text } => Message::new(
                Role::User,
                format!("[Update from {from}. This is not the user speaking.] {text}"),
            ),
        }
    }));
    messages
}

#[async_trait]
impl Thinker for OpenAiThinker {
    async fn think(&self, request: &ThinkRequest, cx: &Ctx) -> Result<ThinkStream, ProviderError> {
        if request.turns.is_empty() {
            return Err(ProviderError::Rejected("nothing to reply to".into()));
        }
        self.client
            .stream(
                &messages(request),
                &request.tools,
                request.must_use_tool,
                self.sampling,
                cx,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use tsetsen_core::ports::ToolCall;

    use super::*;

    #[test]
    fn builds_system_then_turns() {
        let request = ThinkRequest {
            must_use_tool: false,
            instructions: "Be brief.".into(),
            turns: vec![
                Turn::user("hi"),
                Turn::assistant("Hello!"),
                Turn::user("time?"),
            ],
            language: Some("en".into()),
            ..ThinkRequest::default()
        };
        let messages = messages(&request);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::System);
        assert!(messages[0].content.starts_with("Be brief."));
        assert!(messages[0].content.contains("`en`"));
        assert_eq!(
            messages[1..].iter().map(|m| m.role).collect::<Vec<_>>(),
            [Role::User, Role::Assistant, Role::User]
        );
    }

    #[test]
    fn sends_tool_calls_and_their_results_in_wire_format() {
        let request = ThinkRequest {
            must_use_tool: false,
            turns: vec![
                Turn::user("open notes"),
                Turn::Assistant {
                    text: "Opening Notes.".into(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "open_app".into(),
                        arguments: r#"{"name":"Notes"}"#.into(),
                    }],
                },
                Turn::ToolResult {
                    call_id: "call_1".into(),
                    content: "Opened Notes.".into(),
                },
            ],
            ..ThinkRequest::default()
        };
        let json = serde_json::to_value(messages(&request)).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"role": "user", "content": "open notes"},
                {"role": "assistant", "content": "Opening Notes.", "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "open_app", "arguments": "{\"name\":\"Notes\"}"}}
                ]},
                {"role": "tool", "content": "Opened Notes.", "tool_call_id": "call_1"}
            ])
        );
    }
}
