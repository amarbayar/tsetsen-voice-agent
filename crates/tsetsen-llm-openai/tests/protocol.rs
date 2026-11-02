//! Failure modes a healthy local server cannot produce on demand: rate
//! limits, server errors, malformed bodies. Success paths are tested live.

use std::time::Duration;

use serde_json::{Value, json};
use tsetsen_core::ports::{RewriteRequest, Rewriter};
use tsetsen_core::{Ctx, ProviderError, RequestId};
use tsetsen_llm_openai::{ApiKey, OpenAiConfig, OpenAiRewriter};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn server(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

fn config(server: &MockServer) -> OpenAiConfig {
    OpenAiConfig::new("mock", &format!("{}/v1", server.uri()), "m").unwrap()
}

async fn rewrite(config: &OpenAiConfig) -> Result<String, ProviderError> {
    let request = RewriteRequest {
        transcript: "um hello".into(),
        ..RewriteRequest::default()
    };
    let cx = Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(5));
    OpenAiRewriter::new(config)
        .unwrap()
        .rewrite(&request, &cx)
        .await
        .map(|r| r.text)
}

fn completion(content: &str) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": content } }] })
}

#[tokio::test]
async fn maps_http_statuses_to_fallback_semantics() {
    for (status, expected) in [
        (429, "RateLimited"),
        (500, "Unavailable"),
        (503, "Unavailable"),
        (401, "Unavailable"),
        (404, "Unavailable"),
        (400, "Unsupported"),
        (413, "Unsupported"),
    ] {
        let server = server(ResponseTemplate::new(status)).await;
        let error = rewrite(&config(&server)).await.unwrap_err();
        assert!(
            format!("{error:?}").starts_with(expected),
            "{status}: {error:?}"
        );
        assert!(error.allows_fallback(), "{status} should allow fallback");
    }
}

#[tokio::test]
async fn malformed_bodies_are_invalid_responses() {
    for body in [json!("not an object"), json!({ "choices": [] })] {
        let server = server(ResponseTemplate::new(200).set_body_json(body)).await;
        let error = rewrite(&config(&server)).await.unwrap_err();
        assert!(
            matches!(error, ProviderError::InvalidResponse(_)),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn reasoning_without_content_is_reported() {
    let body = json!({ "choices": [{ "message": { "content": "", "reasoning": "Thinking..." } }] });
    let server = server(ResponseTemplate::new(200).set_body_json(body)).await;
    let error = rewrite(&config(&server)).await.unwrap_err();
    assert!(matches!(error, ProviderError::InvalidResponse(m) if m.contains("reasoning_effort")));
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = server(
        ResponseTemplate::new(307).insert_header("location", "https://elsewhere.example/v1"),
    )
    .await;
    let error = rewrite(&config(&server)).await.unwrap_err();
    assert!(matches!(error, ProviderError::Unavailable(_)), "{error:?}");
}

#[tokio::test]
async fn sends_a_deterministic_request_with_the_key_as_a_bearer_token() {
    let server = server(ResponseTemplate::new(200).set_body_json(completion("Hello."))).await;
    let config = config(&server)
        .with_api_key(ApiKey::new("sk-test"))
        .with_reasoning_effort("none");

    assert_eq!(rewrite(&config).await.unwrap(), "Hello.");

    let requests = server.received_requests().await.unwrap();
    let request = &requests[0];
    assert_eq!(
        request.headers.get("authorization").unwrap(),
        "Bearer sk-test"
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], "m");
    assert_eq!(body["temperature"], 0.0);
    assert_eq!(body["stream"], false);
    assert_eq!(body["reasoning_effort"], "none");
    let last = body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(last["content"].as_str().unwrap().contains("um hello"));
}

#[tokio::test]
async fn omits_optional_fields_when_unset() {
    let server = server(ResponseTemplate::new(200).set_body_json(completion("Hello."))).await;
    rewrite(&config(&server)).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("authorization").is_none());
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("reasoning_effort").is_none());
}

mod streaming {
    use futures_util::StreamExt;
    use tsetsen_core::ports::{ThinkEvent, ThinkRequest, Thinker, ToolCall, ToolSpec, Turn};
    use tsetsen_llm_openai::OpenAiThinker;

    use super::*;

    async fn events(
        server: &MockServer,
        tools: Vec<ToolSpec>,
    ) -> Result<Vec<Result<ThinkEvent, ProviderError>>, ProviderError> {
        events_with(server, tools, false).await
    }

    async fn events_with(
        server: &MockServer,
        tools: Vec<ToolSpec>,
        must_use_tool: bool,
    ) -> Result<Vec<Result<ThinkEvent, ProviderError>>, ProviderError> {
        let request = ThinkRequest {
            must_use_tool,
            turns: vec![Turn::user("hello")],
            tools,
            ..ThinkRequest::default()
        };
        let cx = Ctx::new(RequestId::new(1)).with_timeout(Duration::from_secs(5));
        let stream = OpenAiThinker::new(&config(server))
            .unwrap()
            .think(&request, &cx)
            .await?;
        Ok(stream.collect().await)
    }

    /// Text fragments only.
    async fn think(
        server: &MockServer,
    ) -> Result<Vec<Result<String, ProviderError>>, ProviderError> {
        Ok(events(server, Vec::new())
            .await?
            .into_iter()
            .filter_map(|event| match event {
                Ok(ThinkEvent::Text(text)) => Some(Ok(text)),
                Ok(ThinkEvent::ToolCall(_)) => None,
                Err(error) => Some(Err(error)),
            })
            .collect())
    }

    fn open_app_spec() -> ToolSpec {
        ToolSpec {
            name: "open_app".into(),
            description: "Open an app.".into(),
            parameters:
                r#"{"type":"object","properties":{"app":{"type":"string"}},"required":["app"]}"#
                    .into(),
        }
    }

    #[tokio::test]
    async fn offers_tools_and_streams_text_then_the_call() {
        let server = server(sse(&[
            r#"{"choices":[{"delta":{"content":"Opening it."}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"open_app","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"app\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Notes\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]))
        .await;
        let events: Vec<ThinkEvent> = events(&server, vec![open_app_spec()])
            .await
            .unwrap()
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            events,
            [
                ThinkEvent::Text("Opening it.".into()),
                ThinkEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "open_app".into(),
                    arguments: r#"{"app":"Notes"}"#.into(),
                })
            ]
        );
        let sent: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent["tools"][0]["type"], "function");
        assert_eq!(sent["tools"][0]["function"]["name"], "open_app");
        assert_eq!(
            sent["tools"][0]["function"]["parameters"]["required"][0],
            "app"
        );
    }

    #[tokio::test]
    async fn sends_no_tools_field_when_there_are_none() {
        let server = server(sse(&["[DONE]"])).await;
        think(&server).await.unwrap();
        let sent: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert!(sent.get("tools").is_none());
        assert!(sent.get("tool_choice").is_none());
    }

    #[tokio::test]
    async fn requires_a_tool_call_only_when_asked() {
        for (must, expected) in [(true, Some("required")), (false, None)] {
            let server = server(sse(&["[DONE]"])).await;
            events_with(&server, vec![open_app_spec()], must)
                .await
                .unwrap();
            let sent: Value =
                serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
            assert_eq!(sent.get("tool_choice").and_then(Value::as_str), expected);
        }
    }

    #[tokio::test]
    async fn never_emits_a_tool_call_cut_short_by_an_error() {
        let server = server(sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"click","arguments":"{\"elem"}}]}}]}"#,
            r#"{"error":{"message":"overloaded"}}"#,
        ]))
        .await;
        let events = events(&server, vec![open_app_spec()]).await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Err(ProviderError::Unavailable(_))));
    }

    #[tokio::test]
    async fn rejects_a_tool_with_an_invalid_schema_before_sending() {
        let server = server(sse(&["[DONE]"])).await;
        let mut bad = open_app_spec();
        bad.parameters = "{not json".into();
        let error = events(&server, vec![bad]).await.unwrap_err();
        assert!(matches!(error, ProviderError::Rejected(_)), "{error:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    fn sse(events: &[&str]) -> ResponseTemplate {
        let body: String = events.iter().flat_map(|e| ["data: ", e, "\n\n"]).collect();
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }

    #[tokio::test]
    async fn streams_text_and_strips_inline_reasoning() {
        let server = server(sse(&[
            r#"{"choices":[{"delta":{"content":"<think>hm"}}]}"#,
            r#"{"choices":[{"delta":{"content":"m</think>Hi"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"ignored"}}]}"#,
            r#"{"choices":[{"delta":{"content":" there."}}]}"#,
            "[DONE]",
        ]))
        .await;
        let fragments = think(&server).await.unwrap();
        let text: String = fragments.into_iter().map(Result::unwrap).collect();
        assert_eq!(text, "Hi there.");
        let sent: Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent["stream"], true);
    }

    #[tokio::test]
    async fn errors_before_the_stream_allow_fallback() {
        let server = server(ResponseTemplate::new(503)).await;
        let error = think(&server).await.unwrap_err();
        assert!(error.allows_fallback(), "{error:?}");
    }

    #[tokio::test]
    async fn errors_mid_stream_arrive_in_the_stream() {
        let server = server(sse(&[
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"error":{"message":"overloaded"}}"#,
        ]))
        .await;
        let fragments = think(&server).await.unwrap();
        assert_eq!(fragments[0].as_deref().unwrap(), "Hi");
        assert!(matches!(fragments[1], Err(ProviderError::Unavailable(_))));
        assert_eq!(fragments.len(), 2);
    }

    #[tokio::test]
    async fn an_empty_conversation_is_rejected() {
        let server = server(sse(&["[DONE]"])).await;
        let cx = Ctx::new(RequestId::new(1));
        let result = OpenAiThinker::new(&config(&server))
            .unwrap()
            .think(&ThinkRequest::default(), &cx)
            .await;
        assert!(matches!(result, Err(ProviderError::Rejected(_))));
    }
}
