//! Streaming STT client for nemo-speech.cpp's realtime transcription
//! WebSocket (`WS /v1/audio/transcriptions/realtime`; project-specific event
//! protocol — NOT OpenAI Realtime, NOT the VoiceChat protocol).
//!
//! URL/event primitives plus the dial/handshake client live here; the
//! command/event pump follows in a later task.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use harness_core::error::HarnessError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// Build the upstream WS URL: scheme swap (`http://`→`ws://`,
/// `https://`→`wss://`), path append, optional `?api_key=` (percent-encoded).
pub fn ws_url_from_base(base_url: &str, realtime_path: &str, api_key: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    let mut url = format!("{}/{}", ws, realtime_path.trim_start_matches('/'));
    if !api_key.is_empty() {
        url.push_str("?api_key=");
        url.push_str(&percent_encode(api_key));
    }
    url
}

/// RFC 3986 unreserved set only; everything else becomes `%XX`.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// VERIFIED from nemo-speech.cpp http_server.cpp: delta events carry the
/// append increment in field `delta`, completed events the full text in
/// field `transcript`. No cumulative text field exists in this protocol.
fn event_text(v: &serde_json::Value) -> Option<String> {
    v.get("delta")
        .or_else(|| v.get("transcript"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
}

/// Parse one server JSON event into a [`SttRealtimeEvent`], or `None` for
/// events the harness ignores (session lifecycle, unknown types).
pub fn parse_event(v: &serde_json::Value) -> Option<SttRealtimeEvent> {
    match v.get("type")?.as_str()? {
        "conversation.item.input_audio_transcription.delta" => Some(SttRealtimeEvent::Delta {
            text: event_text(v)?,
        }),
        "conversation.item.input_audio_transcription.completed" => {
            Some(SttRealtimeEvent::Completed {
                text: event_text(v)?,
            })
        }
        "input_audio_buffer.committed" => Some(SttRealtimeEvent::Committed),
        "input_audio_buffer.cleared" => Some(SttRealtimeEvent::Cleared),
        _ => None,
    }
}

/// One-shot session configuration (immutable after the first audio frame on
/// the upstream). Empty `language` is omitted = model default.
pub fn session_update_body(
    sample_rate: u32,
    endpointing_ms: u64,
    language: &str,
) -> serde_json::Value {
    let mut session = serde_json::json!({
        "sample_rate": sample_rate,
        "endpointing_ms": endpointing_ms,
        "automatic_punctuation": true,
    });
    if !language.trim().is_empty() {
        session["language"] = serde_json::Value::String(language.to_string());
    }
    serde_json::json!({ "type": "session.update", "session": session })
}

/// Server events the harness acts on. `session.created`/`session.updated` are
/// consumed by the handshake; unknown types are ignored.
#[derive(Debug, Clone, PartialEq)]
pub enum SttRealtimeEvent {
    /// Append increment from the live partial transcript. When the new partial
    /// extends the server's previous one this is the suffix; when it doesn't
    /// (a revision), it is the full revised partial. Accumulate by appending.
    Delta {
        text: String,
    },
    /// Final transcript for an endpointed utterance (upstream endpointing).
    Completed {
        text: String,
    },
    Committed,
    Cleared,
}

/// Handshake/IO timeout for one upstream round trip (dial, greeting, update
/// round trip).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Client-side half of the upstream realtime STT WebSocket. Cheap to clone
/// into tests/constructors; state lives in the [`RealtimeSttSession`] a
/// successful [`RealtimeSttClient::connect`] returns.
#[derive(Debug, Clone)]
pub struct RealtimeSttClient {
    url: String,
    endpointing_ms: u64,
    language: String,
}

impl RealtimeSttClient {
    pub fn new(
        base_url: &str,
        realtime_path: &str,
        api_key: &str,
        endpointing_ms: u64,
        language: &str,
    ) -> Self {
        Self {
            url: ws_url_from_base(base_url, realtime_path, api_key),
            endpointing_ms,
            language: language.to_string(),
        }
    }

    /// Dial the upstream and ride out the handshake: consume the server's
    /// `session.created` greeting, send one `session.update`, await
    /// `session.updated`. An `error` event (nested `error.message`) or the
    /// [`CONNECT_TIMEOUT`] bound fails the handshake.
    pub async fn connect(&self, sample_rate: u32) -> Result<RealtimeSttSession, HarnessError> {
        let (ws, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(&self.url))
            .await
            .map_err(|_| HarnessError::Network("stt realtime dial timeout".to_string()))?
            .map_err(|e| HarnessError::Network(format!("stt realtime dial failed: {e}")))?;
        let mut session = RealtimeSttSession { ws };
        // The server speaks first per docs/api.md; the greeting may still
        // race our update, so tolerate either order.
        session
            .expect_any_of(&["session.created", "session.updated"])
            .await?;
        session
            .send_text(session_update_body(
                sample_rate,
                self.endpointing_ms,
                &self.language,
            ))
            .await?;
        session.expect_any_of(&["session.updated"]).await?;
        Ok(session)
    }
}

/// One live upstream connection (handshake completed). Owned by the pump in
/// production; directly driven by the handshake tests until then.
pub struct RealtimeSttSession {
    ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
}

impl RealtimeSttSession {
    pub async fn send_text(&mut self, v: serde_json::Value) -> Result<(), HarnessError> {
        self.ws
            .send(Message::Text(v.to_string()))
            .await
            .map_err(|e| HarnessError::Network(format!("stt realtime send failed: {e}")))
    }

    pub async fn send_pcm(&mut self, bytes: &[u8]) -> Result<(), HarnessError> {
        self.ws
            .send(Message::Binary(bytes.to_vec()))
            .await
            .map_err(|e| HarnessError::Network(format!("stt realtime send failed: {e}")))
    }

    /// Flush buffered audio upstream (`input_audio_buffer.commit`).
    pub async fn commit(&mut self) -> Result<(), HarnessError> {
        self.send_text(serde_json::json!({"type": "input_audio_buffer.commit"}))
            .await
    }

    /// Discard buffered audio upstream (`input_audio_buffer.clear`).
    pub async fn clear(&mut self) -> Result<(), HarnessError> {
        self.send_text(serde_json::json!({"type": "input_audio_buffer.clear"}))
            .await
    }

    /// Next server text event, skipping binary frames (pings/pongs are
    /// answered by tungstenite on read). `None` = upstream closed.
    pub async fn next_raw(&mut self) -> Option<Result<String, HarnessError>> {
        loop {
            match tokio::time::timeout(CONNECT_TIMEOUT, self.ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => return Some(Ok(t)),
                Ok(Some(Ok(_))) => continue, // Binary/Ping/Pong/Close
                Ok(Some(Err(e))) => {
                    return Some(Err(HarnessError::Network(format!(
                        "stt realtime read failed: {e}"
                    ))))
                }
                Ok(None) | Err(_) => return None, // closed or CONNECT_TIMEOUT elapsed
            }
        }
    }

    /// Read text events (bounded by [`CONNECT_TIMEOUT`]) until one whose
    /// `type` is in `types` arrives; an `error` event fails immediately with
    /// its message surfaced (nested `error.message`, flat `message` fallback).
    async fn expect_any_of(&mut self, types: &[&str]) -> Result<(), HarnessError> {
        loop {
            let text = match self.next_raw().await {
                Some(Ok(t)) => t,
                Some(Err(e)) => return Err(e),
                None => {
                    return Err(HarnessError::Network(
                        "stt realtime upstream closed during handshake".to_string(),
                    ))
                }
            };
            let v: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    return Err(HarnessError::protocol(format!(
                        "bad stt handshake event: {e}"
                    )))
                }
            };
            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if types.contains(&ty) {
                return Ok(());
            }
            if ty == "error" {
                return Err(self.upstream_error(&v));
            }
            // Other event types (late duplicates, buffer confirmations) are
            // ignored while waiting.
        }
    }

    /// Extract an upstream error message (nested `error.message` — VERIFIED
    /// from http_server.cpp — flat `message` fallback, then a generic string).
    fn upstream_error(&self, v: &serde_json::Value) -> HarnessError {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .or_else(|| v.get("message").and_then(|m| m.as_str()))
            .unwrap_or("unknown upstream error");
        HarnessError::upstream("stt-realtime", 0, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_converts_scheme_and_appends_path_and_key() {
        let u = ws_url_from_base(
            "http://127.0.0.1:8080/",
            "/v1/audio/transcriptions/realtime",
            "k",
        );
        assert_eq!(
            u,
            "ws://127.0.0.1:8080/v1/audio/transcriptions/realtime?api_key=k"
        );
        let u = ws_url_from_base(
            "https://asr.example.com",
            "/v1/audio/transcriptions/realtime",
            "",
        );
        assert_eq!(u, "wss://asr.example.com/v1/audio/transcriptions/realtime");
        let u = ws_url_from_base("http://h", "/p", "a b");
        assert_eq!(u, "ws://h/p?api_key=a%20b");
    }

    #[test]
    fn parse_delta_completed_and_buffer_events() {
        // VERIFIED shapes from nemo-speech.cpp http_server.cpp (see plan Context):
        // delta carries the increment in `delta`; completed the full text in
        // `transcript`.
        let d = parse_event(&serde_json::json!({
            "type": "conversation.item.input_audio_transcription.delta", "delta": "hel"
        }))
        .expect("delta");
        assert_eq!(d, SttRealtimeEvent::Delta { text: "hel".into() });
        let c = parse_event(&serde_json::json!({
            "type": "conversation.item.input_audio_transcription.completed", "transcript": "hello."
        }))
        .expect("completed");
        assert_eq!(
            c,
            SttRealtimeEvent::Completed {
                text: "hello.".into()
            }
        );
        assert_eq!(
            parse_event(&serde_json::json!({"type":"input_audio_buffer.committed"})),
            Some(SttRealtimeEvent::Committed)
        );
        assert_eq!(
            parse_event(&serde_json::json!({"type":"input_audio_buffer.cleared"})),
            Some(SttRealtimeEvent::Cleared)
        );
        assert_eq!(
            parse_event(&serde_json::json!({"type":"session.created"})),
            None
        );
        assert_eq!(parse_event(&serde_json::json!({"type":"bogus"})), None);
    }

    #[test]
    fn session_update_carries_rate_endpointing_and_language() {
        let v = session_update_body(16000, 700, "");
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["sample_rate"], 16000);
        assert_eq!(v["session"]["endpointing_ms"], 700);
        // empty = model default
        assert!(v["session"].get("language").is_none());
        let v = session_update_body(24000, 500, "en-US");
        assert_eq!(v["session"]["language"], "en-US");
        assert_eq!(v["session"]["automatic_punctuation"], true);
    }
}
