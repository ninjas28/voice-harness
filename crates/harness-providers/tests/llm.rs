//! Integration tests for the streaming LLM client (wiremock SSE upstream).

use futures::StreamExt;
use harness_providers::llm::{
    ChatMessage, ChatRequest, ChatStream, LlmEvent, LlmProvider, OpenAiLlmClient,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client_for(server: &MockServer) -> OpenAiLlmClient {
    OpenAiLlmClient::new(server.uri(), "/v1/chat/completions", "sk-test")
}

fn chat_req() -> ChatRequest {
    ChatRequest {
        model: "test-model".to_string(),
        messages: vec![ChatMessage::text("user", "hi there")],
        tools: None,
        reasoning_effort: None,
    }
}

fn chunk(delta: serde_json::Value, finish_reason: Option<&str>) -> serde_json::Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
    })
}

fn sse_body(chunks: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

async fn collect(stream: ChatStream) -> Vec<LlmEvent> {
    // Hard bound so a runaway (never-terminating) stream fails the test in
    // seconds instead of consuming unbounded memory.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream
            .map(|r| r.expect("stream event"))
            .collect::<Vec<LlmEvent>>(),
    )
    .await
    .expect("stream terminated within 10s")
}

#[tokio::test]
async fn streams_content_deltas_then_done() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        chunk(json!({ "content": "Hello" }), None),
        chunk(json!({ "content": " world" }), None),
        chunk(json!({}), Some("stop")),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .and(body_partial_json(
            json!({ "stream": true, "model": "test-model" }),
        ))
        .respond_with(sse_response(body))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let events = collect(client.stream_chat(chat_req()).await.expect("stream_chat")).await;

    assert_eq!(
        events,
        vec![
            LlmEvent::Delta("Hello".to_string()),
            LlmEvent::Delta(" world".to_string()),
            LlmEvent::Done {
                finish_reason: Some("stop".to_string())
            },
        ]
    );

    let seen = server.received_requests().await.expect("received requests");
    assert_eq!(seen.len(), 1);
    let auth = seen[0]
        .headers
        .get("authorization")
        .expect("auth header")
        .to_str()
        .expect("ascii header");
    assert_eq!(auth, "Bearer sk-test");
}

#[tokio::test]
async fn accumulates_tool_call_fragments_by_index() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        chunk(
            json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "get_time", "arguments": "" } }] }),
            None,
        ),
        chunk(
            json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"tz\":" } }] }),
            None,
        ),
        chunk(
            json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "\"local\"}" } }] }),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_response(body))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let events = collect(client.stream_chat(chat_req()).await.expect("stream_chat")).await;

    assert_eq!(
        events,
        vec![
            LlmEvent::ToolCall {
                id: "call_1".to_string(),
                name: "get_time".to_string(),
                arguments: "{\"tz\":\"local\"}".to_string(),
            },
            LlmEvent::Done {
                finish_reason: Some("tool_calls".to_string())
            },
        ]
    );
}

#[tokio::test]
async fn falls_back_to_non_stream_when_server_rejects_streaming() {
    let server = MockServer::start().await;
    Mock::given(body_partial_json(json!({ "stream": true })))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            "{\"error\":{\"message\":\"Streaming is not supported by this server\"}}",
        ))
        .mount(&server)
        .await;
    Mock::given(body_partial_json(json!({ "stream": false })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-2",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hi there" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let events = collect(client.stream_chat(chat_req()).await.expect("stream_chat")).await;

    assert_eq!(
        events,
        vec![
            LlmEvent::Delta("Hi there".to_string()),
            LlmEvent::Done {
                finish_reason: Some("stop".to_string())
            },
        ]
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("received requests")
            .len(),
        2
    );
}

#[tokio::test]
async fn stream_terminates_after_sse_body_ends() {
    // Regression: a previous version of the unfold re-flushed `Done` on
    // every poll after the SSE body ended — an infinite stream that OOM'd
    // the machine when collected. Must terminate after Done.
    let server = MockServer::start().await;
    let body = sse_body(&[chunk(json!({ "content": "hello" }), None)]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_response(body))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let events = collect(client.stream_chat(chat_req()).await.expect("stream_chat")).await;
    assert_eq!(
        events,
        vec![
            LlmEvent::Delta("hello".to_string()),
            LlmEvent::Done {
                finish_reason: None
            },
        ]
    );
}

#[tokio::test]
async fn reasoning_effort_is_sent_when_set_and_omitted_when_none() {
    // Set: the key must reach the wire.
    let server = MockServer::start().await;
    let body = sse_body(&[chunk(json!({ "content": "ok" }), None)]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "reasoning_effort": "high",
            "stream": true,
            "model": "test-model"
        })))
        .respond_with(sse_response(body.clone()))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let mut req = chat_req();
    req.reasoning_effort = Some("high".to_string());
    let events = collect(client.stream_chat(req).await.expect("stream_chat")).await;
    assert!(matches!(events.last(), Some(LlmEvent::Done { .. })));

    // Unset: the key must not appear in the JSON body at all.
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_response(body.clone()))
        .mount(&server2)
        .await;
    let client2 = client_for(&server2);
    let _ = collect(client2.stream_chat(chat_req()).await.expect("stream_chat")).await;
    let seen = server2
        .received_requests()
        .await
        .expect("received requests")
        .remove(0);
    let sent: serde_json::Value = serde_json::from_slice(&seen.body).expect("json body");
    assert!(
        sent.get("reasoning_effort").is_none(),
        "reasoning_effort must be omitted when unset: {sent}"
    );
}

#[tokio::test]
async fn upstream_error_without_stream_hint_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(body_partial_json(json!({ "stream": true })))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = match client.stream_chat(chat_req()).await {
        Err(e) => e,
        Ok(_) => panic!("expected upstream error"),
    };

    let msg = err.to_string();
    assert!(msg.contains("500"), "unexpected error: {msg}");
    assert!(msg.contains("boom"), "unexpected error: {msg}");
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("received requests")
            .len(),
        1
    );
}
