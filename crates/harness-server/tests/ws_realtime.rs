//! Realtime WebSocket integration tests: a real axum server on 127.0.0.1:0
//! with wiremock upstreams. Full VAD turn, speech.end shortcut, malformed
//! input resilience. Every receive is timeout-wrapped.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use harness_core::config::Config;
use harness_plugins::PluginRegistry;
use harness_providers::llm::{ChatRequest, ChatStream, LlmProvider};
use harness_providers::stt::SttProvider;
use harness_providers::tts::TtsProvider;
use harness_server::http::{build_router, RouterDeps};
use harness_server::state::SessionStore;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

type Ws = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------- mocks
// (Same wrap-the-real-client pattern as http_api.rs.)

struct MockLlm {
    url: String,
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn stream_chat(
        &self,
        req: ChatRequest,
    ) -> Result<ChatStream, harness_core::error::HarnessError> {
        let client = harness_providers::llm::OpenAiLlmClient::new(
            self.url.clone(),
            "/v1/chat/completions",
            "test-llm-key",
        );
        client.stream_chat(req).await
    }
}

struct MockTts {
    url: String,
}

#[async_trait]
impl TtsProvider for MockTts {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, harness_core::error::HarnessError> {
        let client = harness_providers::tts::OpenAiTtsClient::new(
            self.url.clone(),
            "/v1/audio/speech",
            "test-tts-key",
            "magpie",
            "default",
            "wav",
        );
        client.synthesize(text).await
    }
}

struct MockStt {
    url: String,
}

#[async_trait]
impl SttProvider for MockStt {
    async fn transcribe(
        &self,
        pcm16k: &[i16],
    ) -> Result<String, harness_core::error::HarnessError> {
        let client = harness_providers::stt::OpenAiSttClient::new(
            self.url.clone(),
            "/v1/audio/transcriptions",
            "test-stt-key",
            "nemotron",
        );
        client.transcribe(pcm16k).await
    }
}

// ---------------------------------------------------------------- helpers

fn llm_sse(chunks: &[&str]) -> String {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "c1",
                "object": "chat.completion.chunk",
                "model": "m",
                "choices": [{ "index": 0, "delta": { "content": c }, "finish_reason": null }]
            })
        ));
    }
    body.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "c1",
            "object": "chat.completion.chunk",
            "model": "m",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
        })
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

/// 30 ms of loud frequency-hopping sine (480 samples @ 16 kHz) — WebRTC VAD
/// flags it. A constant tone gets adapted away by the VAD's background
/// estimator, so the frequency changes every frame.
fn speech_frame() -> Vec<i16> {
    thread_local! {
        static N: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    let n = N.with(|c| c.get());
    N.with(|c| c.set(n + 1));
    let freqs = [220.0, 440.0, 880.0, 660.0, 330.0, 550.0];
    let f = freqs[n % freqs.len()];
    let phase = (n as f32 * 37.7) % std::f32::consts::TAU;
    (0..480)
        .map(|i| {
            ((2.0 * std::f32::consts::PI * f * (i as f32 + phase) / 16_000.0).sin() * 9000.0) as i16
        })
        .collect()
}

/// 30 ms of near-silence.
fn silence_frame() -> Vec<i16> {
    vec![0i16; 480]
}

fn audio_msg(pcm: &[i16]) -> String {
    serde_json::json!({
        "type": "audio.data",
        "pcm": base64::engine::general_purpose::STANDARD.encode(
            pcm.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>()
        )
    })
    .to_string()
}

/// One axum server + three wiremock upstreams.
async fn spawn_server_with_keys(api_keys: Vec<String>) -> (String, Config) {
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(llm_sse(&[
            "The quick brown fox jumps over the lazy dog. ",
            "Then it wandered off to nap under the old oak tree.",
        ])))
        .mount(&llm)
        .await;

    let tts = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(
            harness_providers::stt::wav_from_pcm(&vec![100i16; 160], 16_000),
        ))
        .mount(&tts)
        .await;

    let stt = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "text": "what time is it" })),
        )
        .mount(&stt)
        .await;

    let mut config = Config::default();
    config.server.api_keys = api_keys;
    let deps = RouterDeps {
        config: config.clone(),
        llm: Arc::new(MockLlm { url: llm.uri() }),
        tts: Arc::new(MockTts { url: tts.uri() }),
        stt: Arc::new(MockStt { url: stt.uri() }),
        plugins: Arc::new(PluginRegistry::new()),
        sessions: Arc::new(SessionStore::new()),
    };
    let app = build_router(deps);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("ws://{addr}/v1/realtime"), config)
}

/// Send one text frame.
async fn send_text(ws: &mut Ws, s: String) {
    timeout(TEST_TIMEOUT, ws.send(Message::Text(s)))
        .await
        .expect("send within timeout")
        .unwrap();
}

/// Receive one server text message with a hard timeout, parsed as JSON.
async fn recv_json(ws: &mut Ws) -> serde_json::Value {
    loop {
        let msg = timeout(TEST_TIMEOUT, ws.next())
            .await
            .expect("receive within timeout — server hung")
            .expect("stream open")
            .expect("no ws error");
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).expect("server sends valid json");
        }
        // Binary/Ping/Pong/Close: skip (Close would end the stream on next poll).
    }
}

fn type_of(v: &serde_json::Value) -> &str {
    v["type"].as_str().unwrap_or("?")
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn full_turn_vad_to_turn_completed() {
    let (url, _config) = spawn_server_with_keys(Vec::new()).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start", "device_id": "esp32-kitchen" }).to_string(),
    )
    .await;

    // Stream ~1.2 s of speech (40 × 30 ms) to open the utterance…
    for _ in 0..40 {
        send_text(&mut ws, audio_msg(&speech_frame())).await;
    }
    // …then ~1.2 s of silence (40 frames) so the 700 ms endpoint fires
    // (4 leading zero frames read as speech right after speech; add headroom).
    for _ in 0..40 {
        send_text(&mut ws, audio_msg(&silence_frame())).await;
    }

    // Expected: State(speech) on VAD open, then after the endpoint:
    // Transcript → State(thinking) → deltas → AudioChunks in seq order →
    // ResponseText → State(speaking) → TurnCompleted.
    let mut saw_speech_state = false;
    let mut saw_transcript = false;
    let mut saw_thinking = false;
    let mut seqs: Vec<u32> = Vec::new();
    let mut saw_response_text = false;
    let mut saw_completed = false;

    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "state" => match v["state"].as_str().unwrap() {
                "speech" => saw_speech_state = true,
                "thinking" => saw_thinking = true,
                _ => {}
            },
            "transcript" => {
                saw_transcript = true;
                assert_eq!(v["text"], "what time is it");
            }
            "response.text" => saw_response_text = true,
            "audio.chunk" => seqs.push(v["seq"].as_u64().unwrap() as u32),
            "turn.completed" => {
                assert!(!saw_completed, "only one turn.completed per turn");
                saw_completed = true;
                break;
            }
            "error" => panic!("unexpected error frame: {v}"),
            _ => {}
        }
    }

    assert!(saw_speech_state, "server emits State(speech) on VAD open");
    assert!(saw_transcript, "transcript precedes the response");
    assert!(saw_thinking, "State(thinking) while the LLM runs");
    assert!(saw_response_text, "full response text arrives");
    assert_eq!(seqs, vec![0, 1], "audio chunks stream in seq order");
    assert!(saw_completed, "turn.completed terminates the turn");
}

#[tokio::test]
async fn speech_end_forces_utterance_end() {
    let (url, _config) = spawn_server_with_keys(Vec::new()).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;

    // 15 frames of speech (450 ms) then an explicit speech.end — no silence
    // tail needed. 450 ms clears the 300 ms min-utterance bar.
    for _ in 0..15 {
        send_text(&mut ws, audio_msg(&speech_frame())).await;
    }
    send_text(
        &mut ws,
        serde_json::json!({ "type": "speech.end" }).to_string(),
    )
    .await;

    let mut saw_transcript = false;
    let mut saw_completed = false;
    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "transcript" => {
                saw_transcript = true;
                assert_eq!(v["text"], "what time is it");
            }
            "turn.completed" => {
                assert!(!saw_completed, "only one turn.completed per turn");
                saw_completed = true;
                break;
            }
            "error" => panic!("unexpected error frame: {v}"),
            _ => {}
        }
    }
    assert!(
        saw_transcript && saw_completed,
        "speech.end shortcut completes the turn"
    );
}

#[tokio::test]
async fn malformed_json_yields_error_and_connection_stays_open() {
    let (url, _config) = spawn_server_with_keys(Vec::new()).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;

    // Drain the session.start acknowledgment before sending garbage so the
    // next frame received is deterministically the Error reply.
    let ack = recv_json(&mut ws).await;
    assert_eq!(
        type_of(&ack),
        "state",
        "session.start acknowledged with a state"
    );

    // Garbage frame.
    send_text(&mut ws, "this is not json {{{".to_string()).await;

    let v = recv_json(&mut ws).await;
    assert_eq!(type_of(&v), "error", "malformed input → Error frame");
    assert!(
        !v["code"].as_str().unwrap_or("").is_empty(),
        "error carries a code: {v}"
    );

    // Connection still open: subsequent valid messages work. Feed a real
    // utterance and drive a turn to completion.
    for _ in 0..15 {
        send_text(&mut ws, audio_msg(&speech_frame())).await;
    }
    send_text(
        &mut ws,
        serde_json::json!({ "type": "speech.end" }).to_string(),
    )
    .await;

    let mut saw_completed = false;
    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "error" => panic!("connection broke after malformed input: {v}"),
            "turn.completed" => {
                assert!(!saw_completed, "only one turn.completed per turn");
                saw_completed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_completed, "connection remains fully usable");
}

#[tokio::test]
async fn oversized_client_chunks_are_split_for_vad() {
    // Regression: the macOS client sends ~300 ms (4832-sample) audio chunks.
    // WebRTC VAD only accepts 10/20/30 ms frames and reports silence for any
    // other size — so the server MUST split incoming chunks into ≤30 ms
    // sub-frames before feeding the assembler, or no utterance ever opens.
    let (url, _config) = spawn_server_with_keys(Vec::new()).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start", "device_id": "macos-client" }).to_string(),
    )
    .await;

    // ~1.2 s of speech as 8 chunks of 4832 samples (302 ms each), the exact
    // shape the macOS client produces from its AVAudioEngine tap.
    for _ in 0..8 {
        let big: Vec<i16> = (0..16).flat_map(|_| speech_frame()).collect();
        assert_eq!(big.len(), 7680); // 16 × 480 = 480 ms — yes, even bigger
        send_text(&mut ws, audio_msg(&big)).await;
    }
    // ~1 s of trailing silence, also chunky.
    for _ in 0..4 {
        let silence: Vec<i16> = vec![0i16; 7680];
        send_text(&mut ws, audio_msg(&silence)).await;
    }

    // The turn must complete exactly as with well-formed 30 ms frames.
    let mut saw_transcript = false;
    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "transcript" => {
                saw_transcript = true;
                assert_eq!(v["text"], "what time is it");
            }
            "turn.completed" => break,
            "error" => panic!("unexpected error frame: {v}"),
            _ => {}
        }
    }
    assert!(
        saw_transcript,
        "server splits oversized chunks; VAD still fires"
    );
}

#[tokio::test]
async fn auth_required_when_keys_configured() {
    let (url, _config) = spawn_server_with_keys(vec!["secret".into()]).await;

    // No token → the upgrade must be refused.
    let result = tokio_tungstenite::connect_async(&url).await;
    assert!(result.is_err(), "missing token is refused");

    // ?token= works.
    let ws: Ws = tokio_tungstenite::connect_async(format!("{url}?token=secret"))
        .await
        .expect("token auth accepted")
        .0;
    drop(ws);

    // Authorization header also works.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.clone().into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer secret".parse().unwrap());
    let ws: Ws = tokio_tungstenite::connect_async(req)
        .await
        .expect("bearer auth accepted")
        .0;
    drop(ws);
}
