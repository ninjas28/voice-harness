//! Shared harness for the harness-server integration tests: a real axum
//! server on 127.0.0.1:0 with wiremock LLM/TTS/STT upstreams, in-process
//! mock providers, and timeout-bounded WS client helpers. `allow(dead_code)`
//! because each test binary uses a subset of the helpers.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use harness_core::config::Config;
use harness_plugins::PluginRegistry;
use harness_providers::llm::{ChatRequest, ChatStream, LlmProvider};
use harness_providers::stt::SttProvider;
use harness_providers::stt_realtime::RealtimeSttClient;
use harness_providers::tts::TtsProvider;
use harness_server::http::{build_router, RouterDeps};
use harness_server::state::SessionStore;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const TEST_TIMEOUT: Duration = Duration::from_secs(10);

pub type Ws = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------- mocks
// (Same wrap-the-real-client pattern as http_api.rs.)

pub struct MockLlm {
    url: String,
    requests: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn stream_chat(
        &self,
        req: ChatRequest,
    ) -> Result<ChatStream, harness_core::error::HarnessError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let client = harness_providers::llm::OpenAiLlmClient::new(
            self.url.clone(),
            "/v1/chat/completions",
            "test-llm-key",
        );
        client.stream_chat(req).await
    }
}

/// LLM mock that RECORDS the last outgoing [`ChatRequest`] before delegating
/// to the real client (same wrap-the-real-client pattern as [`MockLlm`]). For
/// tests that must assert exactly what text reached the LLM. `Clone` shares
/// the same underlying recorder (Arc fields).
#[derive(Clone)]
pub struct RecordingLlm {
    url: String,
    pub last_request: Arc<Mutex<Option<ChatRequest>>>,
    pub requests: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for RecordingLlm {
    async fn stream_chat(
        &self,
        req: ChatRequest,
    ) -> Result<ChatStream, harness_core::error::HarnessError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        *self.last_request.lock().expect("recording llm lock") = Some(req.clone());
        let client = harness_providers::llm::OpenAiLlmClient::new(
            self.url.clone(),
            "/v1/chat/completions",
            "test-llm-key",
        );
        client.stream_chat(req).await
    }
}

pub struct MockTts {
    url: String,
}

#[async_trait]
impl TtsProvider for MockTts {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, harness_core::error::HarnessError> {
        let client = harness_providers::tts::OpenAiTtsClient::new(
            self.url.clone(),
            "/v1/audio/speech",
            "test-tts-key",
            "tts-model",
            "default",
            "wav",
        );
        client.synthesize(text).await
    }
}

/// Consecutive transcripts for STT calls (script of one = reused). Script
/// consumption is serialized through the wiremock responder's mutex.
pub struct ScriptedStt {
    queue: Mutex<VecDeque<String>>,
}

#[async_trait]
impl SttProvider for ScriptedStt {
    async fn transcribe(
        &self,
        _pcm16k: &[i16],
    ) -> Result<String, harness_core::error::HarnessError> {
        let mut q = self.queue.lock().unwrap();
        if q.len() == 1 {
            return Ok(q[0].clone());
        }
        Ok(q.pop_front().unwrap_or_default())
    }
}

// ---------------------------------------------------------------- helpers

pub fn llm_sse(chunks: &[&str]) -> String {
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
pub fn speech_frame() -> Vec<i16> {
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
pub fn silence_frame() -> Vec<i16> {
    vec![0i16; 480]
}

pub fn audio_msg(pcm: &[i16]) -> String {
    serde_json::json!({
        "type": "audio.data",
        "pcm": base64::engine::general_purpose::STANDARD.encode(
            pcm.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>()
        )
    })
    .to_string()
}

/// One axum server + three wiremock upstreams. `transcripts` are the
/// consecutive STT results (last one repeats); `customize` mutates the config
/// before the router is built (session knobs, keys…); `stt_realtime` wires
/// the streaming-STT client (`None` = batch mode). `llm_spy` receives the
/// [`RecordingLlm`] the router is built with (spy on outgoing chat requests);
/// `None` = plain [`MockLlm`].
pub async fn spawn_server(
    mut config: Config,
    transcripts: Vec<String>,
    customize: impl FnOnce(&mut Config),
    stt_realtime: Option<Arc<RealtimeSttClient>>,
    llm_spy: Option<std::sync::mpsc::Sender<RecordingLlm>>,
) -> (String, Arc<AtomicUsize>) {
    let llm = MockServer::builder().start().await;
    let llm_requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|_req: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_string(llm_sse(&[
                "The quick brown fox jumps over the lazy dog. ",
                "Then it wandered off to nap under the old oak tree.",
            ]))
        })
        .mount(&llm)
        .await;

    let tts = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(
            harness_providers::stt::wav_from_pcm(&vec![100i16; 160], 16_000),
        ))
        .mount(&tts)
        .await;

    // STT is scripted in-process by ScriptedStt below — the wiremock endpoint
    // only exists so the router has a URL to point at; it is never called.
    let mut queue: VecDeque<String> = transcripts.into_iter().collect();
    if queue.is_empty() {
        queue.push_back("what time is it".to_string());
    }
    let stt = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "text": "wiremock-stt-unused" })),
        )
        .mount(&stt)
        .await;

    customize(&mut config);
    // Optional LLM spy: when a spy channel is provided, the router gets a
    // [`RecordingLlm`] and the caller receives a clone of it through the
    // channel (sync send before the server task starts — no race).
    let llm_provider: Arc<dyn LlmProvider> = match llm_spy {
        Some(spy) => {
            let recorder = RecordingLlm {
                url: llm.uri(),
                last_request: Arc::new(Mutex::new(None)),
                requests: Arc::clone(&llm_requests),
            };
            spy.send(recorder.clone())
                .expect("llm spy receiver must stay open");
            Arc::new(recorder)
        }
        None => Arc::new(MockLlm {
            url: llm.uri(),
            requests: llm_requests.clone(),
        }),
    };
    let deps = RouterDeps {
        router: std::sync::Arc::new(harness_server::state::FederationRouter::new()),
        identities: std::sync::Arc::new(tokio::sync::RwLock::new(
            harness_server::identity::IdentityRegistry::default(),
        )),
        config: config.clone(),
        llm: llm_provider,
        tts: Arc::new(MockTts { url: tts.uri() }),
        stt: Arc::new(ScriptedStt {
            queue: Mutex::new(queue),
        }),
        stt_realtime,
        plugins: Arc::new(PluginRegistry::new()),
        sessions: Arc::new(SessionStore::new()),
    };
    let app = build_router(deps);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Keep the wiremock servers alive for the whole test: without this task
    // they would drop the moment `spawn_server` returns, and a dropped
    // MockServer either recycles (pooled) or shuts down (dedicated) — both
    // make the later LLM/TTS requests 404/connection-refuse mid-test.
    tokio::spawn(async move {
        std::future::pending::<()>().await;
        drop((llm, tts, stt));
    });

    (format!("ws://{addr}/v1/realtime"), llm_requests)
}

pub async fn spawn_server_with_keys(api_keys: Vec<String>) -> (String, Config) {
    let mut config = Config::default();
    config.server.api_keys = api_keys;
    let (url, _) = spawn_server(config.clone(), Vec::new(), |_| {}, None, None).await;
    (url, config)
}

/// Send one text frame.
pub async fn send_text(ws: &mut Ws, s: String) {
    timeout(TEST_TIMEOUT, ws.send(Message::Text(s)))
        .await
        .expect("send within timeout")
        .unwrap();
}

/// Receive one server text message with a hard timeout, parsed as JSON.
pub async fn recv_json(ws: &mut Ws) -> serde_json::Value {
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

pub fn type_of(v: &serde_json::Value) -> &str {
    v["type"].as_str().unwrap_or("?")
}
