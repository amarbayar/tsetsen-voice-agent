//! Server-sent events from a streaming chat completion.

use serde::Deserialize;
use tsetsen_core::ProviderError;

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Default, Deserialize)]
struct Delta {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    /// A JSON string per the spec; some servers send the object itself.
    arguments: Option<serde_json::Value>,
}

/// What one event contained.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Reply text. Reasoning deltas are never surfaced.
    Text(String),
    /// Part of a tool call. Servers may split one call across many events.
    ToolCall(ToolCallFragment),
    /// The server finished the reply.
    Done,
}

/// One piece of a tool call, keyed by its position in the reply.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolCallFragment {
    pub(crate) index: usize,
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) arguments: String,
}

/// Assembles tool calls from fragments.
#[derive(Default)]
pub(crate) struct ToolCalls {
    calls: Vec<ToolCallFragment>,
}

impl ToolCalls {
    /// The most calls one reply may make; more is a runaway model.
    const MAX: usize = 16;

    pub(crate) fn push(&mut self, fragment: ToolCallFragment) -> Result<(), ProviderError> {
        if fragment.index >= Self::MAX {
            return Err(ProviderError::InvalidResponse(
                "too many tool calls in one reply".into(),
            ));
        }
        if self.calls.len() <= fragment.index {
            self.calls
                .resize_with(fragment.index + 1, ToolCallFragment::default);
        }
        let call = &mut self.calls[fragment.index];
        if fragment.id.is_some() {
            call.id = fragment.id;
        }
        if let Some(name) = fragment.name {
            call.name.get_or_insert_with(String::new).push_str(&name);
        }
        call.arguments.push_str(&fragment.arguments);
        Ok(())
    }

    /// The finished calls, in order. Calls without a name are dropped.
    pub(crate) fn finish(&mut self) -> Vec<tsetsen_core::ports::ToolCall> {
        std::mem::take(&mut self.calls)
            .into_iter()
            .enumerate()
            .filter_map(|(i, call)| {
                let name = call.name.filter(|n| !n.trim().is_empty())?;
                let arguments = if call.arguments.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    call.arguments
                };
                Some(tsetsen_core::ports::ToolCall {
                    id: call.id.unwrap_or_else(|| format!("call_{i}")),
                    name: name.trim().to_owned(),
                    arguments,
                })
            })
            .collect()
    }
}

/// Splits a byte stream into `data:` payloads, across arbitrary chunk
/// boundaries.
#[derive(Default)]
pub(crate) struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    /// Feeds bytes; returns the events completed by them, in order. An error
    /// ends the list, after any events that preceded it in the same bytes.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<Result<Event, ProviderError>> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            let Some(data) = line.strip_prefix("data:") else {
                continue; // blank separators, comments, `event:` lines
            };
            match parse(data.trim_start()) {
                Ok(parsed) => events.extend(parsed.into_iter().map(Ok)),
                Err(error) => {
                    events.push(Err(error));
                    break;
                }
            }
        }
        events
    }
}

fn parse(data: &str) -> Result<Vec<Event>, ProviderError> {
    if data == "[DONE]" {
        return Ok(vec![Event::Done]);
    }
    let chunk: Chunk = serde_json::from_str(data)
        .map_err(|e| ProviderError::InvalidResponse(format!("malformed stream chunk: {e}")))?;
    if let Some(error) = chunk.error {
        return Err(ProviderError::Unavailable(format!("stream error: {error}")));
    }
    let Some(delta) = chunk.choices.into_iter().next().map(|c| c.delta) else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
        events.push(Event::Text(text));
    }
    for call in delta.tool_calls.unwrap_or_default() {
        let (name, arguments) = call
            .function
            .map_or((None, None), |f| (f.name, f.arguments));
        let arguments = match arguments {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(text)) => text,
            Some(object) => object.to_string(),
        };
        events.push(Event::ToolCall(ToolCallFragment {
            index: call.index,
            id: call.id,
            name,
            arguments,
        }));
    }
    Ok(events)
}

/// Removes `<think>…</think>` blocks from streamed text, across fragment
/// boundaries, for models that inline their reasoning.
#[derive(Default)]
pub(crate) struct ThinkFilter {
    pending: String,
    thinking: bool,
}

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

impl ThinkFilter {
    /// Returns the speakable part of `fragment`; holds back a possible
    /// partial tag until the next fragment decides it.
    pub(crate) fn push(&mut self, fragment: &str) -> String {
        self.pending.push_str(fragment);
        let mut out = String::new();
        loop {
            let tag = if self.thinking { CLOSE } else { OPEN };
            if let Some(at) = self.pending.find(tag) {
                if !self.thinking {
                    out.push_str(&self.pending[..at]);
                }
                self.pending.drain(..at + tag.len());
                self.thinking = !self.thinking;
                continue;
            }
            // Keep any suffix that could be the start of the tag.
            let keep = (1..tag.len())
                .rev()
                .find(|&n| self.pending.ends_with(&tag[..n]))
                .unwrap_or(0);
            let cut = self.pending.len() - keep;
            if !self.thinking {
                out.push_str(&self.pending[..cut]);
            }
            self.pending.drain(..cut);
            return out;
        }
    }

    /// Flushes what is left at the end of the stream.
    pub(crate) fn finish(&mut self) -> String {
        if self.thinking {
            self.pending.clear();
            String::new()
        } else {
            std::mem::take(&mut self.pending)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(text: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
            serde_json::json!(text)
        )
    }

    #[test]
    fn decodes_events_split_anywhere() {
        let stream = format!(
            "{}{}: keep-alive\n\ndata: [DONE]\n\n",
            chunk("Hel"),
            chunk("lo!")
        );
        for size in 1..stream.len() {
            let mut decoder = Decoder::default();
            let mut events = Vec::new();
            for piece in stream.as_bytes().chunks(size) {
                events.extend(decoder.feed(piece).into_iter().map(Result::unwrap));
            }
            assert_eq!(
                events,
                [
                    Event::Text("Hel".into()),
                    Event::Text("lo!".into()),
                    Event::Done
                ],
                "chunk size {size}"
            );
        }
    }

    #[test]
    fn ignores_reasoning_and_empty_deltas() {
        let mut decoder = Decoder::default();
        let events = decoder.feed(
            b"data: {\"choices\":[{\"delta\":{\"reasoning\":\"hmm\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
        );
        assert!(events.is_empty());
    }

    #[test]
    fn reports_errors_after_the_text_before_them() {
        let bytes = format!(
            "{}data: {{\"error\":{{\"message\":\"overloaded\"}}}}\n{}",
            chunk("Hi"),
            chunk("lost")
        );
        let events = Decoder::default().feed(bytes.as_bytes());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].as_ref().unwrap(), &Event::Text("Hi".into()));
        assert!(matches!(events[1], Err(ProviderError::Unavailable(_))));
        assert!(matches!(
            Decoder::default().feed(b"data: not json\n")[..],
            [Err(ProviderError::InvalidResponse(_))]
        ));
    }

    #[test]
    fn assembles_tool_calls_sent_whole_or_in_pieces() {
        // Ollama: the whole call in one delta, after the text.
        let whole = "data: {\"choices\":[{\"delta\":{\"content\":\"On it.\"}}]}\n\n\
            data: {\"choices\":[{\"delta\":{\"content\":\"\",\"tool_calls\":[{\"id\":\"call_1\",\"index\":0,\
            \"type\":\"function\",\"function\":{\"name\":\"open_app\",\"arguments\":\"{\\\"name\\\":\\\"Notes\\\"}\"}}]}}]}\n\n\
            data: [DONE]\n\n";
        // OpenAI and vLLM: id and name first, then argument fragments.
        let pieces = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\
            \"function\":{\"name\":\"open_app\",\"arguments\":\"\"}}]}}]}\n\n\
            data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"na\"}}]}}]}\n\n\
            data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"me\\\":\\\"Notes\\\"}\"}}]}}]}\n\n\
            data: [DONE]\n\n";
        for stream in [whole, pieces] {
            let mut calls = ToolCalls::default();
            for event in Decoder::default().feed(stream.as_bytes()) {
                if let Event::ToolCall(fragment) = event.unwrap() {
                    calls.push(fragment).unwrap();
                }
            }
            assert_eq!(
                calls.finish(),
                [tsetsen_core::ports::ToolCall {
                    id: "call_1".into(),
                    name: "open_app".into(),
                    arguments: r#"{"name":"Notes"}"#.into(),
                }]
            );
        }
    }

    #[test]
    fn accepts_object_arguments_and_fills_in_missing_ids() {
        let stream = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[\
            {\"function\":{\"name\":\"list_windows\"}},\
            {\"index\":1,\"function\":{\"name\":\"open_app\",\"arguments\":{\"name\":\"Mail\"}}},\
            {\"index\":2,\"function\":{\"arguments\":\"{}\"}}]}}]}\n";
        let mut calls = ToolCalls::default();
        for event in Decoder::default().feed(stream) {
            if let Event::ToolCall(fragment) = event.unwrap() {
                calls.push(fragment).unwrap();
            }
        }
        let calls = calls.finish();
        assert_eq!(calls.len(), 2, "the nameless call is dropped");
        assert_eq!(
            (calls[0].id.as_str(), calls[0].arguments.as_str()),
            ("call_0", "{}")
        );
        assert_eq!(calls[1].arguments, r#"{"name":"Mail"}"#);
        let mut runaway = ToolCalls::default();
        assert!(
            runaway
                .push(ToolCallFragment {
                    index: 99,
                    ..ToolCallFragment::default()
                })
                .is_err()
        );
    }

    #[test]
    fn strips_think_blocks_across_fragments() {
        let text = "<think>plan the answer</think>Sure, it's 4 PM.";
        for size in 1..text.len() {
            let mut filter = ThinkFilter::default();
            let mut out = String::new();
            let mut rest = text;
            while !rest.is_empty() {
                let at = rest.char_indices().nth(size).map_or(rest.len(), |(i, _)| i);
                out.push_str(&filter.push(&rest[..at]));
                rest = &rest[at..];
            }
            out.push_str(&filter.finish());
            assert_eq!(out, "Sure, it's 4 PM.", "fragment size {size}");
        }
    }

    #[test]
    fn keeps_text_that_merely_looks_like_a_tag() {
        let mut filter = ThinkFilter::default();
        let mut out = filter.push("a < b and <th");
        out.push_str(&filter.push("ings>"));
        out.push_str(&filter.finish());
        assert_eq!(out, "a < b and <things>");
    }

    #[test]
    fn drops_an_unclosed_think_block() {
        let mut filter = ThinkFilter::default();
        let mut out = filter.push("Hi. <think>never closed");
        out.push_str(&filter.finish());
        assert_eq!(out, "Hi. ");
    }
}
