//! The HTTP client shared by every capability this adapter implements.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tsetsen_core::ports::{ThinkEvent, ThinkStream, ToolSpec};
use tsetsen_core::{Ctx, Interrupted, ProviderError};
use url::Url;

use crate::config::{ApiKey, ConfigError, OpenAiConfig};
use crate::sse::{Decoder, Event, ThinkFilter, ToolCalls};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Message {
    pub(crate) role: Role,
    pub(crate) content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
}

impl Message {
    pub(crate) fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// A tool call as the chat completions API spells it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WireToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: WireFunction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WireFunction {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolSpec<'a>,
}

#[derive(Serialize)]
struct WireToolSpec<'a> {
    name: &'a str,
    description: &'a str,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    messages: &'a [Message],
    temperature: f32,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

#[derive(Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: Option<String>,
    /// Ollama calls it `reasoning`; vLLM and others `reasoning_content`.
    #[serde(default, alias = "reasoning_content")]
    reasoning: Option<String>,
}

/// How to sample a completion.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sampling {
    pub(crate) temperature: f32,
    pub(crate) max_tokens: u32,
}

pub(crate) struct ChatClient {
    http: reqwest::Client,
    endpoint: Url,
    model: String,
    api_key: Option<ApiKey>,
    timeout: Duration,
    reasoning_effort: Option<String>,
}

impl ChatClient {
    pub(crate) fn new(config: &OpenAiConfig) -> Result<Self, ConfigError> {
        let http = reqwest::Client::builder()
            // A redirect could forward the bearer token, and the transcript, to
            // a host the egress policy never approved.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            endpoint: config.endpoint("chat/completions")?,
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            timeout: config.request_timeout,
            reasoning_effort: config.reasoning_effort.clone(),
        })
    }

    /// Sends a completion request and checks the status. The timeout covers
    /// the whole response, body included, and never outlives the deadline.
    async fn send(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        must_use_tool: bool,
        sampling: Sampling,
        stream: bool,
        cx: &Ctx,
    ) -> Result<(reqwest::Response, bool), ProviderError> {
        cx.check()?;
        let tools = tools
            .iter()
            .map(|tool| {
                let parameters = serde_json::from_str(&tool.parameters).map_err(|e| {
                    ProviderError::Rejected(format!("tool `{}` has invalid schema: {e}", tool.name))
                })?;
                Ok(WireTool {
                    kind: "function",
                    function: WireToolSpec {
                        name: &tool.name,
                        description: &tool.description,
                        parameters,
                    },
                })
            })
            .collect::<Result<Vec<_>, ProviderError>>()?;
        let (timeout, limited_by_deadline) = match cx.remaining() {
            Some(left) if left < self.timeout => (left, true),
            _ => (self.timeout, false),
        };
        let body = Body {
            model: &self.model,
            messages,
            temperature: sampling.temperature,
            max_tokens: sampling.max_tokens,
            stream,
            reasoning_effort: self.reasoning_effort.as_deref(),
            tool_choice: (must_use_tool && !tools.is_empty()).then_some("required"),
            tools,
        };
        let mut request = self
            .http
            .post(self.endpoint.clone())
            .timeout(timeout)
            .json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key.expose());
        }
        let response = request
            .send()
            .await
            .map_err(|e| transport_error(&e, limited_by_deadline))?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(status));
        }
        Ok((response, limited_by_deadline))
    }

    /// A streaming completion: reply text as it is generated, with any
    /// reasoning removed, then any tool calls. Dropping the stream aborts the
    /// request.
    pub(crate) async fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        must_use_tool: bool,
        sampling: Sampling,
        cx: &Ctx,
    ) -> Result<ThinkStream, ProviderError> {
        struct State<S> {
            bytes: S,
            decoder: Decoder,
            filter: ThinkFilter,
            calls: ToolCalls,
            done: bool,
        }
        let (response, limited_by_deadline) = self
            .send(messages, tools, must_use_tool, sampling, true, cx)
            .await?;
        let state = State {
            bytes: response.bytes_stream().boxed(),
            decoder: Decoder::default(),
            filter: ThinkFilter::default(),
            calls: ToolCalls::default(),
            done: false,
        };
        // Each network read becomes a batch: the text it completed, then any
        // tool calls (once the reply is complete), then any error, so nothing
        // that arrived before an error is lost. Half-built tool calls are
        // dropped on error: running a truncated call would be worse.
        let batches = futures_util::stream::unfold(state, move |mut state| async move {
            if state.done {
                return None;
            }
            let events = match state.bytes.next().await {
                Some(Ok(bytes)) => state.decoder.feed(&bytes),
                Some(Err(error)) => vec![Err(transport_error(&error, limited_by_deadline))],
                None => vec![Ok(Event::Done)],
            };
            let mut text = String::new();
            let mut calls = Vec::new();
            let mut error = None;
            for event in events {
                match event {
                    Ok(Event::Text(fragment)) => text.push_str(&state.filter.push(&fragment)),
                    Ok(Event::ToolCall(fragment)) => {
                        if let Err(e) = state.calls.push(fragment) {
                            error = Some(e);
                            state.done = true;
                            break;
                        }
                    }
                    Ok(Event::Done) => {
                        text.push_str(&state.filter.finish());
                        calls = state.calls.finish();
                        state.done = true;
                        break;
                    }
                    Err(e) => {
                        error = Some(e);
                        state.done = true;
                        break;
                    }
                }
            }
            let batch: Vec<Result<ThinkEvent, ProviderError>> = (!text.is_empty())
                .then_some(Ok(ThinkEvent::Text(text)))
                .into_iter()
                .chain(calls.into_iter().map(|c| Ok(ThinkEvent::ToolCall(c))))
                .chain(error.map(Err))
                .collect();
            Some((futures_util::stream::iter(batch), state))
        });
        let fragments = batches.flatten();
        Ok(Box::pin(fragments))
    }

    /// One deterministic, non-streaming completion. Returns the assistant text,
    /// which may be empty.
    pub(crate) async fn complete(
        &self,
        messages: &[Message],
        max_tokens: u32,
        cx: &Ctx,
    ) -> Result<String, ProviderError> {
        let sampling = Sampling {
            temperature: 0.0,
            max_tokens,
        };
        let (response, limited_by_deadline) =
            self.send(messages, &[], false, sampling, false, cx).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| transport_error(&e, limited_by_deadline))?;
        let completion: Completion = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::InvalidResponse(format!("malformed completion: {e}")))?;
        let message = completion
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::InvalidResponse("completion has no choices".into()))?
            .message;

        match message.content.filter(|text| !text.trim().is_empty()) {
            Some(text) => Ok(text),
            None if message.reasoning.is_some_and(|r| !r.trim().is_empty()) => {
                Err(ProviderError::InvalidResponse(
                    "model returned only reasoning; set reasoning_effort to \"none\"".into(),
                ))
            }
            None => Ok(String::new()),
        }
    }
}

fn status_error(status: StatusCode) -> ProviderError {
    match status.as_u16() {
        429 => ProviderError::RateLimited,
        400 | 413 | 422 => ProviderError::Unsupported(format!("HTTP {status}")),
        401 | 403 => ProviderError::Unavailable(format!("authentication failed (HTTP {status})")),
        404 => ProviderError::Unavailable(format!("endpoint or model not found (HTTP {status})")),
        _ => ProviderError::Unavailable(format!("HTTP {status}")),
    }
}

fn transport_error(error: &reqwest::Error, limited_by_deadline: bool) -> ProviderError {
    match (error.is_timeout(), limited_by_deadline) {
        (true, true) => ProviderError::Interrupted(Interrupted::DeadlineExceeded),
        (true, false) => ProviderError::Unavailable("request timed out".into()),
        (false, _) => ProviderError::Unavailable(error.to_string()),
    }
}
