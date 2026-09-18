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
async fn tool_call_fragment_with_absurd_index_is_a_protocol_error() {
    // Regression: `index` comes from upstream JSON and is prompt-injectable.
    // ensure_slot used to grow the accumulator Vec until the index resolved —
    // an index of 100_000 means 100k allocations (and effectively unbounded
    // growth for larger indices). Must map to a protocol error instead.
    let server = MockServer::start().await;
    let body = sse_body(&[chunk(
        json!({ "tool_calls": [{ "index": 100000, "id": "call_x", "type": "function",
            "function": { "name": "get_time", "arguments": "" } }] }),
        None,
    )]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_response(body))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client
            .stream_chat(chat_req())
            .await
            .expect("stream_chat opens")
            .collect::<Vec<Result<LlmEvent, _>>>(),
    )
    .await
    .expect("stream terminates within 10s (no hang/OOM)");

    // The absurd index must surface as an Err event, not a hang or OOM.
    let err = result
        .into_iter()
        .find_map(|r| r.err())
        .expect("absurd tool-call index must fail the stream");
    let text = err.to_string();
    assert!(
        text.contains("protocol error") && text.contains("tool_call"),
        "expected a protocol error naming tool_call slots, got: {text}"
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

/// Regression: a stalled SSE stream (upstream stops sending mid-answer, but
/// never closes the connection) used to hang `stream_chat` forever — the
/// reqwest client only set `connect_timeout`, so the turn task froze and the
/// end-of-turn `turn.completed` burst never went out; clients stuck in
/// `speaking` (see the stalled-turn lesson in AGENTS.md). Each read from the
/// stream must be bounded so a silent upstream errors the turn instead of
/// hanging it. A total request timeout is deliberately NOT set: a long but
/// actively-streaming answer is legitimate.
///
/// wiremock cannot stall mid-body (its `set_delay` holds the whole response),
/// so this test scripts a raw TCP server: first SSE chunk immediately, then
/// silence — the connection stays open, exactly the failure mode.
#[tokio::test]
async fn stalled_sse_stream_errors_instead_of_hanging() {
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    // One accepted connection: HTTP/1.1 + chunked SSE. First event at once,
    // then nothing — the socket stays open (the stalled-upstream condition).
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let sse = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
        let head = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n";
        sock.write_all(head.as_bytes()).await.expect("head");
        sock.write_all(format!("{:x}\r\n{sse}\r\n", sse.len()).as_bytes())
            .await
            .expect("first chunk");
        sock.flush().await.expect("flush");
        // Then silence for 30 s: the client's read timeout must fire long
        // before this task would send anything again or close.
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    // Short knob: per-read timeout well inside the 10 s test bound.
    let client = OpenAiLlmClient::with_read_timeout(
        format!("http://{addr}"),
        "/v1/chat/completions",
        "sk-test",
        Duration::from_millis(500),
    );

    let started = std::time::Instant::now();
    let mut stream = client.stream_chat(chat_req()).await.expect("stream_chat");
    // First event flows.
    let first = stream
        .next()
        .await
        .expect("first event must arrive before the stall")
        .expect("first event ok");
    assert!(matches!(first, LlmEvent::Delta(_)), "unexpected: {first:?}");
    // Next read must error on the stalled stream, well inside the bound.
    let err = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("second read must resolve within the 10s test bound (read timeout must fire)")
        .expect("stream must not end cleanly on a stall")
        .expect_err("stalled stream must error, not hang");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "read timeout must fire promptly, took {:?}",
        started.elapsed()
    );
    let msg = err.to_string();
    assert!(msg.contains("llm"), "unexpected: {msg}");
    assert!(msg.contains("stalled"), "unexpected: {msg}");
}
