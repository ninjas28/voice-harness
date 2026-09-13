//! OpenAI-compatible streaming chat client.
//!
//! Sends a chat-completion request with `stream: true` and parses the SSE
//! response (`data: {chunk}` lines terminated by `data: [DONE]`) into
//! [`LlmEvent`]s. Tool-call argument fragments arriving across multiple
//! chunks are accumulated here (not by the caller): one complete
//! [`LlmEvent::ToolCall`] is emitted when the stream finishes.
//!
//! Fallback: if the upstream rejects streaming (error mentioning "stream"),
//! the request is retried once with `stream: false` and the whole message is
//! returned as a single [`LlmEvent::Delta`] + [`LlmEvent::Done`].
//!
//! Real-server path probe (plan note): both `/api/chat/completions` and
//! `/v1/chat/completions` stay configurable via `llm.chat_path`; probing with
//! the real key happens in Task 14, not here.

use std::pin::Pin;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use harness_core::error::HarnessError;

/// Stream of LLM events produced by [`LlmProvider::stream_chat`].
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<LlmEvent, HarnessError>> + Send>>;

/// One message in a chat conversation (OpenAI chat format subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    /// A plain text message (system / user / assistant).
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
}

/// Chat completion request: model + messages + optional OpenAI tool specs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    /// OpenAI-style reasoning effort ("minimal"|"low"|"medium"|"high").
    /// `None` = the key is omitted from the request entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// One streamed event from a chat completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    /// Incremental text content.
    Delta(String),
    /// A completed tool call (argument fragments accumulated by the client).
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// End of stream with the model's finish reason.
    Done { finish_reason: Option<String> },
}

/// Streaming chat-completion provider.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn stream_chat(&self, req: ChatRequest) -> Result<ChatStream, HarnessError>;
}

/// OpenAI-compatible client.
pub struct OpenAiLlmClient {
    http: reqwest::Client,
    base_url: String,
    chat_path: String,
    api_key: String,
}

/// One in-progress tool call being accumulated from streamed fragments.
#[derive(Debug, Default, Clone)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// State machine mapping SSE chunks onto [`LlmEvent`]s, accumulating
/// tool-call fragments and emitting them once complete.
#[derive(Debug, Default)]
struct ChunkMapper {
    tool_acc: Vec<ToolCallAcc>,
    finish_reason: Option<String>,
}

impl ChunkMapper {
    /// Grow the accumulator vector so `index` always resolves.
    fn ensure_slot(&mut self, idx: usize) {
        while self.tool_acc.len() <= idx {
            self.tool_acc.push(ToolCallAcc::default());
        }
    }

    /// Apply one SSE `data:` payload; returns the events it maps to
    /// (without `Done`, which is appended by [`Self::finish`]).
    fn apply(&mut self, data: &str) -> Result<Vec<LlmEvent>, HarnessError> {
        if data.trim() == "[DONE]" {
            return Ok(Vec::new()); // stream end handled by the caller
        }

        let chunk: StreamChunk = serde_json::from_str(data)
            .map_err(|e| HarnessError::protocol(format!("bad llm chunk: {e}")))?;

        let mut events = Vec::new();
        let Some(choice) = chunk.choices.into_iter().next() else {
            return Ok(events);
        };

        if let Some(delta) = choice.delta {
            if let Some(text) = delta.content.filter(|s| !s.is_empty()) {
                events.push(LlmEvent::Delta(text));
            }
            for frag in delta.tool_calls.into_iter().flatten() {
                self.ensure_slot(frag.index);
                let acc = &mut self.tool_acc[frag.index];
                if let Some(id) = frag.id {
                    acc.id = id;
                }
                if let Some(f) = frag.function {
                    if let Some(name) = f.name {
                        acc.name = name;
                    }
                    if let Some(args) = f.arguments {
                        acc.arguments.push_str(&args);
                    }
                }
            }
        }
        if choice.finish_reason.is_some() {
            self.finish_reason = choice.finish_reason;
        }
        Ok(events)
    }

    /// Emit accumulated tool calls (in index order) plus the final `Done`
    /// with the last-seen finish reason, at stream end.
    fn finish(&self) -> Vec<LlmEvent> {
        let mut events: Vec<LlmEvent> = self
            .tool_acc
            .iter()
            .filter(|acc| !acc.name.is_empty())
            .map(|acc| LlmEvent::ToolCall {
                id: acc.id.clone(),
                name: acc.name.clone(),
                arguments: acc.arguments.clone(),
            })
            .collect();
        events.push(LlmEvent::Done {
            finish_reason: self.finish_reason.clone(),
        });
        events
    }
}

/// Subset of OpenAI's `chat.completion.chunk` wire shape.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallFragment>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallFragment {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ToolCallFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct ToolCallFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

fn body_of(message: &ChatMessage) -> String {
    message.content.clone().unwrap_or_default()
}

/// Parse a non-streamed chat-completion JSON body into events.
fn parse_non_stream(body: &str) -> Result<Vec<LlmEvent>, HarnessError> {
    #[derive(Deserialize)]
    struct NonStream {
        choices: Vec<NonStreamChoice>,
    }
    #[derive(Deserialize)]
    struct NonStreamChoice {
        message: Option<ChatMessage>,
        #[serde(default)]
        finish_reason: Option<String>,
    }

    let parsed: NonStream = serde_json::from_str(body)
        .map_err(|e| HarnessError::protocol(format!("bad llm response: {e}")))?;

    let mut events = Vec::new();
    if let Some(choice) = parsed.choices.into_iter().next() {
        if let Some(message) = choice.message {
            let text = body_of(&message);
            if !text.is_empty() {
                events.push(LlmEvent::Delta(text));
            }
            if let Some(tool_calls) = message.tool_calls {
                for tc in tool_calls {
                    let id = tc
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let name = tc
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let arguments = tc
                        .pointer("/function/arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if !name.is_empty() {
                        events.push(LlmEvent::ToolCall {
                            id,
                            name,
                            arguments,
                        });
                    }
                }
            }
        }
        events.push(LlmEvent::Done {
            finish_reason: choice.finish_reason,
        });
    }
    Ok(events)
}

/// How the upstream body should be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    /// SSE event stream.
    Sse,
    /// A single JSON document (non-stream fallback).
    Json,
}

/// Classify the response body. `text/event-stream` and `application/json`
/// content types are decided from the header alone (nothing consumed); an
/// ambiguous content type (test servers can force `text/plain`) is decided
/// by peeking the first chunk, which is returned so the caller can prepend
/// it back — bytes pulled via `chunk()` are not replayed by `text()`.
async fn classify_body(mut resp: reqwest::Response) -> (BodyKind, bytes::Bytes, reqwest::Response) {
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ct.starts_with("text/event-stream") {
        return (BodyKind::Sse, bytes::Bytes::new(), resp);
    }
    if ct.contains("json") {
        return (BodyKind::Json, bytes::Bytes::new(), resp);
    }
    match resp.chunk().await {
        Ok(Some(chunk)) if chunk.first() == Some(&b'{') => (BodyKind::Json, chunk, resp),
        Ok(Some(chunk)) => (BodyKind::Sse, chunk, resp),
        _ => (BodyKind::Json, bytes::Bytes::new(), resp),
    }
}

/// A byte stream that yields an already-peeked prefix chunk before the
/// remaining chunks of the wrapped stream.
struct PrefixStream {
    prefix: Option<bytes::Bytes>,
    inner: futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
}

impl futures::Stream for PrefixStream {
    type Item = Result<bytes::Bytes, reqwest::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if let Some(prefix) = self.prefix.take() {
            return std::task::Poll::Ready(Some(Ok(prefix)));
        }
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl PrefixStream {
    fn new(
        prefix: bytes::Bytes,
        inner: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    ) -> Self {
        Self {
            prefix: (!prefix.is_empty()).then_some(prefix),
            inner: Box::pin(inner),
        }
    }
}

/// Boxed stream of SSE `data:` payload strings.
type DataStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<String, HarnessError>> + Send>>;

/// Wrap an SSE byte stream into a stream of `data:` payload strings.
fn sse_data_stream<S>(byte_stream: S) -> DataStream
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    use eventsource_stream::Eventsource as _;
    Box::pin(byte_stream.eventsource().map(|res| match res {
        Ok(event) => Ok(event.data.to_string()),
        Err(e) => Err(HarnessError::protocol(format!("llm SSE error: {e}"))),
    }))
}

/// Convert a ready list of events into a [`ChatStream`].
fn events_stream(events: Vec<LlmEvent>) -> ChatStream {
    Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
}

impl OpenAiLlmClient {
    pub fn new(
        base_url: impl Into<String>,
        chat_path: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client builds"),
            base_url: base_url.into(),
            chat_path: chat_path.into(),
            api_key: api_key.into(),
        }
    }

    fn url(&self) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), self.chat_path)
    }

    fn body_json(&self, req: &ChatRequest, stream: bool) -> serde_json::Value {
        let mut body = serde_json::to_value(req).expect("ChatRequest serializes");
        body["stream"] = serde_json::Value::Bool(stream);
        body
    }

    fn authed(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }
        builder
    }

    /// POST the chat request. On a streaming request whose failure mentions
    /// "stream", retry exactly once with `stream: false`.
    async fn post(
        &self,
        req: &ChatRequest,
        stream: bool,
    ) -> Result<reqwest::Response, HarnessError> {
        let resp = self
            .authed(self.http.post(self.url()))
            .json(&self.body_json(req, stream))
            .send()
            .await
            .map_err(|e| HarnessError::Network(format!("llm request failed: {e}")))?;

        if resp.status().is_success() {
            return Ok(resp);
        }

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if stream && body.to_lowercase().contains("stream") {
            tracing::warn!(status, body = %body, "upstream rejected streaming; retrying non-streaming");
            return Box::pin(self.post(req, false)).await;
        }

        Err(HarnessError::upstream("llm", status, body))
    }
}

#[async_trait]
impl LlmProvider for OpenAiLlmClient {
    async fn stream_chat(&self, req: ChatRequest) -> Result<ChatStream, HarnessError> {
        let resp = self.post(&req, true).await?;

        // Classify the response: SSE (streaming) vs a JSON document
        // (non-stream fallback). Peek one chunk and prepend it so no bytes
        // are lost, then parse accordingly.
        let (kind, peeked, resp) = classify_body(resp).await;
        if kind == BodyKind::Json {
            let body = resp.text().await.unwrap_or_default();
            return Ok(events_stream(parse_non_stream(&body)?));
        }

        // Fold the SSE data payloads through a [`ChunkMapper`], then append
        // the accumulated tool calls + `Done` at stream end. Implemented as
        // an explicit state-machine unfold (no locks): the queue buffers the
        // events each payload maps to, and the mapper carries tool-call
        // accumulator state across payloads.
        let stream = futures::stream::unfold(
            (
                ChunkMapper::default(),
                Vec::new(),
                Some(sse_data_stream(PrefixStream::new(
                    peeked,
                    resp.bytes_stream(),
                ))) as Option<DataStream>,
            ),
            |(mut mapper, mut queue, mut data)| async move {
                loop {
                    if let Some(ev) = (!queue.is_empty()).then(|| queue.remove(0)) {
                        return Some((Ok(ev), (mapper, queue, data)));
                    }
                    let Some(mut stream) = data.take() else {
                        return None; // fully drained after end-of-stream flush
                    };
                    match stream.as_mut().next().await {
                        Some(Ok(d)) => match mapper.apply(&d) {
                            Ok(events) => {
                                data = Some(stream);
                                queue = events;
                            }
                            Err(e) => return Some((Err(e), (mapper, queue, data))),
                        },
                        Some(Err(e)) => return Some((Err(e), (mapper, queue, data))),
                        None => {
                            // Byte stream ended: flush accumulated tool calls
                            // + Done, and drop the exhausted stream so the
                            // next poll terminates instead of looping.
                            let mut events = mapper.finish();
                            return Some((Ok(events.remove(0)), (mapper, events, None)));
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream) as ChatStream)
    }
}
