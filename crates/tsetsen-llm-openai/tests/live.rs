//! Against a real OpenAI-compatible server (Ollama by default).
//!
//! ```sh
//! ollama pull qwen3.5:9b-mlx
//! cargo test -p tsetsen-llm-openai --test live -- --ignored
//! ```
//!
//! Override the endpoint with `TSETSEN_LIVE_BASE_URL` and `TSETSEN_LIVE_MODEL`.

use std::time::{Duration, Instant};

use tsetsen_core::ports::{RewriteRequest, Rewriter};
use tsetsen_core::{Ctx, Describe, Interrupted, Locality, ProviderError, RequestId};
use tsetsen_engine::{OverlapGuard, RewriteCheck, RewriteGuard};
use tsetsen_llm_openai::{OpenAiConfig, OpenAiRewriter};

fn base_url() -> String {
    std::env::var("TSETSEN_LIVE_BASE_URL").unwrap_or_else(|_| "http://localhost:11434/v1".into())
}

fn model() -> String {
    std::env::var("TSETSEN_LIVE_MODEL").unwrap_or_else(|_| "qwen3.5:9b-mlx".into())
}

fn rewriter_for(model: &str) -> OpenAiRewriter {
    let config = OpenAiConfig::new("live", &base_url(), model)
        .unwrap()
        .with_reasoning_effort("none");
    OpenAiRewriter::new(&config).unwrap()
}

fn cx() -> Ctx {
    Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(60))
}

async fn clean(transcript: &str) -> String {
    let request = RewriteRequest {
        transcript: transcript.into(),
        ..RewriteRequest::default()
    };
    let rewriter = rewriter_for(&model());
    // The first call may load the model into memory; time a second one.
    rewriter.rewrite(&request, &cx()).await.unwrap();
    let started = Instant::now();
    let text = rewriter.rewrite(&request, &cx()).await.unwrap().text;
    eprintln!(
        "{:>5} ms  {transcript:?} -> {text:?}",
        started.elapsed().as_millis()
    );
    text
}

fn guard_accepts(raw: &str, rewritten: &str) -> bool {
    OverlapGuard::default().accepts(&RewriteCheck {
        raw,
        rewritten,
        vocabulary: &[],
    })
}

#[tokio::test]
#[ignore = "needs a local OpenAI-compatible server; see module docs"]
async fn a_local_endpoint_is_described_as_local() {
    assert_eq!(
        rewriter_for(&model()).descriptor().locality,
        Locality::Local
    );
}

#[tokio::test]
#[ignore = "needs a local OpenAI-compatible server; see module docs"]
async fn resolves_a_self_correction() {
    let raw = "um so lets meet at 3 no wait 4pm on thursday";
    let text = clean(raw).await;
    let lower = text.to_lowercase();
    assert!(
        lower.contains('4') && lower.contains("thursday"),
        "{text:?}"
    );
    assert!(!lower.contains('3') && !lower.contains("wait"), "{text:?}");
    assert!(guard_accepts(raw, &text), "guard rejected {text:?}");
}

#[tokio::test]
#[ignore = "needs a local OpenAI-compatible server; see module docs"]
async fn writes_a_question_out_instead_of_answering_it() {
    let raw = "what's the uh capital of mongolia";
    let text = clean(raw).await;
    assert!(
        text.to_lowercase().contains("capital of mongolia"),
        "{text:?}"
    );
    assert!(!text.to_lowercase().contains("ulaanbaatar"), "{text:?}");
    assert!(guard_accepts(raw, &text), "guard rejected {text:?}");
}

#[tokio::test]
#[ignore = "needs a local OpenAI-compatible server; see module docs"]
async fn an_unknown_model_is_unavailable_so_the_router_can_fall_back() {
    let request = RewriteRequest {
        transcript: "hello".into(),
        ..RewriteRequest::default()
    };
    let error = rewriter_for("no-such-model:latest")
        .rewrite(&request, &cx())
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Unavailable(_)), "{error:?}");
    assert!(error.allows_fallback());
}

#[tokio::test]
#[ignore = "needs a local OpenAI-compatible server; see module docs"]
async fn the_request_deadline_is_honored() {
    let request = RewriteRequest {
        transcript: "write this out in full please ".repeat(50),
        ..RewriteRequest::default()
    };
    let cx = Ctx::new(RequestId::new(1)).with_timeout(Duration::from_millis(20));
    let error = rewriter_for(&model())
        .rewrite(&request, &cx)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ProviderError::Interrupted(Interrupted::DeadlineExceeded)
        ),
        "{error:?}"
    );
}

#[tokio::test]
#[ignore = "needs a local OpenAI-compatible server; see module docs"]
async fn hybrid_thinking_models_fail_loudly_without_reasoning_effort() {
    let config = OpenAiConfig::new("live", &base_url(), model()).unwrap();
    let request = RewriteRequest {
        transcript: "um hello there".into(),
        ..RewriteRequest::default()
    };
    match OpenAiRewriter::new(&config)
        .unwrap()
        .rewrite(&request, &cx())
        .await
    {
        // Qwen3.5 on Ollama: reasoning consumes the budget and no text comes back.
        Err(ProviderError::InvalidResponse(message)) => {
            assert!(message.contains("reasoning_effort"), "{message}");
        }
        // Models without default thinking simply answer.
        Ok(rewrite) => assert_ne!(rewrite.text, ""),
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

mod thinker {
    use std::sync::Arc;

    use futures_util::StreamExt;
    use tsetsen_core::ports::{ThinkEvent, ThinkRequest, Thinker, ToolCall, Turn};
    use tsetsen_engine::{DesktopTools, Toolbox, WORKER_INSTRUCTIONS};
    use tsetsen_llm_openai::OpenAiThinker;
    use tsetsen_testkit::FakeDesktop;

    use super::*;

    fn thinker() -> OpenAiThinker {
        let config = OpenAiConfig::new("live", &base_url(), model())
            .unwrap()
            .with_reasoning_effort("none");
        OpenAiThinker::new(&config).unwrap().with_temperature(0.0)
    }

    /// Streams a reply; returns (time to first fragment, fragments, text).
    async fn reply(turns: Vec<Turn>) -> (Duration, usize, String) {
        let request = ThinkRequest {
            instructions: "You are a voice assistant. Reply in one or two short spoken sentences."
                .into(),
            turns,
            ..ThinkRequest::default()
        };
        let started = Instant::now();
        let mut stream = thinker().think(&request, &cx()).await.unwrap();
        let (mut first, mut fragments, mut text) = (None, 0, String::new());
        while let Some(fragment) = stream.next().await {
            first.get_or_insert_with(|| started.elapsed());
            fragments += 1;
            if let ThinkEvent::Text(fragment) = fragment.unwrap() {
                text.push_str(&fragment);
            }
        }
        let first = first.expect("at least one fragment");
        eprintln!(
            "first fragment {first:?}, {fragments} fragments, total {:?}: {text:?}",
            started.elapsed()
        );
        (first, fragments, text)
    }

    #[tokio::test]
    #[ignore = "needs a local OpenAI-compatible server; see module docs"]
    async fn streams_a_reply_in_many_fragments_quickly() {
        reply(vec![Turn::user("hi")]).await; // load the model
        let (first, fragments, text) =
            reply(vec![Turn::user("What is the capital of Mongolia?")]).await;
        assert!(
            text.contains("Ulaanbaatar") || text.contains("Ulan Bator"),
            "{text:?}"
        );
        assert!(fragments > 3, "not streamed: {fragments} fragments");
        assert!(
            first < Duration::from_millis(800),
            "first fragment after {first:?}"
        );
        assert!(!text.contains("<think>"), "{text:?}");
    }

    #[tokio::test]
    #[ignore = "needs a local OpenAI-compatible server; see module docs"]
    async fn remembers_the_conversation() {
        let (_, _, text) = reply(vec![
            Turn::user("My dog's name is Biscuit."),
            Turn::assistant("Biscuit is a lovely name."),
            Turn::user("What's my dog called?"),
        ])
        .await;
        assert!(text.contains("Biscuit"), "{text:?}");
    }

    /// Runs one step with the real desktop tools offered; returns the text
    /// and the calls.
    async fn step(turns: Vec<Turn>) -> (String, Vec<ToolCall>) {
        let tools = DesktopTools::new(Arc::new(FakeDesktop::verified()));
        let request = ThinkRequest {
            instructions: WORKER_INSTRUCTIONS.into(),
            turns,
            tools: tools.specs(),
            ..ThinkRequest::default()
        };
        let started = Instant::now();
        let mut stream = thinker().think(&request, &cx()).await.unwrap();
        let (mut text, mut calls) = (String::new(), Vec::new());
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                ThinkEvent::Text(fragment) => text.push_str(&fragment),
                ThinkEvent::ToolCall(call) => calls.push(call),
            }
        }
        eprintln!("{:?}: {text:?} {calls:?}", started.elapsed());
        (text, calls)
    }

    fn argument(call: &ToolCall, key: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(&call.arguments).unwrap();
        value[key].as_str().unwrap_or_default().to_lowercase()
    }

    #[tokio::test]
    #[ignore = "needs a local OpenAI-compatible server; see module docs"]
    async fn calls_the_right_tool_when_asked_to_act() {
        let (_, calls) = step(vec![Turn::user("Can you open Chrome for me?")]).await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].name, "open_app");
        assert!(argument(&calls[0], "app").contains("chrome"), "{calls:?}");

        // Reading directly, listing its windows or bringing Safari forward
        // first are all sound.
        let (_, calls) = step(vec![Turn::user("What's on my screen in Safari?")]).await;
        assert!(
            matches!(
                calls.first().map(|c| c.name.as_str()),
                Some("read_window" | "open_app" | "list_windows")
            ),
            "{calls:?}"
        );
        if calls[0].name != "list_windows" {
            assert!(argument(&calls[0], "app").contains("safari"));
        }
    }

    #[tokio::test]
    #[ignore = "needs a local OpenAI-compatible server; see module docs"]
    async fn just_talks_when_no_action_is_needed() {
        let (text, calls) = step(vec![Turn::user("What's the capital of Mongolia?")]).await;
        assert!(calls.is_empty(), "{calls:?}");
        assert!(
            text.contains("Ulaanbaatar") || text.contains("Ulan Bator"),
            "{text}"
        );
    }

    #[tokio::test]
    #[ignore = "needs a local OpenAI-compatible server; see module docs"]
    async fn reports_a_tool_result_instead_of_calling_again() {
        let (text, calls) = step(vec![
            Turn::user("Open Notes."),
            Turn::Assistant {
                text: "Opening Notes.".into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "open_app".into(),
                    arguments: r#"{"app":"Notes"}"#.into(),
                }],
            },
            Turn::ToolResult {
                call_id: "call_1".into(),
                content: "Notes is open and in front, showing \"Notes\".".into(),
            },
        ])
        .await;
        assert!(calls.is_empty(), "{calls:?}");
        assert!(!text.trim().is_empty());
    }

    #[tokio::test]
    #[ignore = "needs a local OpenAI-compatible server; see module docs"]
    async fn asks_for_approval_when_a_result_says_to() {
        let (text, calls) = step(vec![
            Turn::user("Send it."),
            Turn::Assistant {
                text: "Sending it now.".into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "press_keys".into(),
                    arguments: r#"{"keys":"return","app":"Slack"}"#.into(),
                }],
            },
            Turn::ToolResult {
                call_id: "call_1".into(),
                content: "Not done yet. This needs the user's OK: ask them in a few words whether \
                          to press return in Slack, then stop."
                    .into(),
            },
        ])
        .await;
        assert!(calls.is_empty(), "{calls:?}");
        assert!(text.contains('?'), "asks a question: {text}");
    }
}
