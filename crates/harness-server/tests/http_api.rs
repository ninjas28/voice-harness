//! HTTP API integration tests: wiremock upstreams + in-process router via
//! `tower::ServiceExt::oneshot`. Covers text turns, WAV ingest, auth, and
//! content-type rejection. Every response body read is inside a timeout.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use harness_core::config::Config;
use harness_plugins::PluginRegistry;
use harness_providers::llm::{ChatRequest, ChatStream, LlmProvider};
use harness_providers::stt::SttProvider;
use harness_providers::tts::TtsProvider;
use harness_server::http::{build_router, RouterDeps};
use harness_server::state::SessionStore;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// ---------------------------------------------------------------- mock LLM

struct MockLlm {
    url: String,
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn stream_chat(
        &self,
        req: ChatRequest,
    ) -> Result<ChatStream, harness_core::error::HarnessError> {
        // Real client against the wiremock upstream: exercises the SSE path
        // end-to-end rather than scripting the trait directly.
        let client = harness_providers::llm::OpenAiLlmClient::new(
            self.url.clone(),
            "/v1/chat/completions",
            "test-llm-key",
        );
        client.stream_chat(req).await
    }
}

// ---------------------------------------------------------------- mock TTS

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
            "tts-model",
            "default",
            "wav",
        );
        client.synthesize(text).await
    }
}

// ---------------------------------------------------------------- mock STT

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
            "asr-model",
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

/// Minimal valid WAV wrapping `pcm` at 16 kHz (server's own encoder).
fn wav_body(pcm: &[i16]) -> Vec<u8> {
    harness_providers::stt::wav_from_pcm(pcm, 16_000)
}

/// A tiny real WAV for TTS to return (RIFF/PCM/mono/16k, N samples).
fn tts_wav_response(samples: usize) -> Vec<u8> {
    harness_providers::stt::wav_from_pcm(&vec![100i16; samples], 16_000)
}

async fn spawn_upstreams() -> (MockServer, MockServer, MockServer) {
    let llm = MockServer::start().await;
    let tts = MockServer::start().await;
    let stt = MockServer::start().await;

    // LLM: two content deltas then finish (SSE). Each delta is a full
    // sentence (≥ 30 chars) so the chunker emits one audio chunk per delta.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(llm_sse(&[
            "The quick brown fox jumps over the lazy dog. ",
            "Then it wandered off to nap under the old oak tree.",
        ])))
        .mount(&llm)
        .await;

    // TTS: a small WAV per synthesis.
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tts_wav_response(160)))
        .mount(&tts)
        .await;

    // STT: transcript JSON.
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "text": "what time is it" })),
        )
        .mount(&stt)
        .await;

    (llm, tts, stt)
}

async fn oneshot(
    deps: RouterDeps,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Result<axum::response::Response, std::convert::Infallible> {
    let app = build_router(deps);
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = match body {
        Some(b) => req.body(Body::from(b)),
        None => req.body(Body::empty()),
    }
    .unwrap();
    app.oneshot(req).await
}

fn deps_for(llm: &MockServer, tts: &MockServer, stt: &MockServer) -> RouterDeps {
    let mut config = Config::default();
    config.server.api_keys = Vec::new(); // no auth required
    RouterDeps {
        config,
        llm: Arc::new(MockLlm { url: llm.uri() }),
        tts: Arc::new(MockTts { url: tts.uri() }),
        stt: Arc::new(MockStt { url: stt.uri() }),
        stt_realtime: None,
        plugins: Arc::new(PluginRegistry::new()),
        sessions: Arc::new(SessionStore::new()),
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn text_turn_returns_populated_response() {
    let (llm, tts, stt) = spawn_upstreams().await;
    let deps = deps_for(&llm, &tts, &stt);

    let resp = oneshot(
        deps,
        "POST",
        "/v1/turn",
        &[("content-type", "application/json")],
        Some(br#"{"text":"hi","device_id":"esp32-kitchen"}"#.to_vec()),
    )
    .await
    .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        TEST_TIMEOUT,
        axum::body::to_bytes(resp.into_body(), 1 << 20),
    )
    .await
    .expect("body read completes")
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(v["transcript"], "hi", "text passthrough as transcript");
    assert_eq!(
        v["response_text"],
        "The quick brown fox jumps over the lazy dog. Then it wandered off to nap under the old oak tree."
    );
    let audio = v["audio_wav_base64"].as_str().unwrap();
    assert!(audio.len() > 44, "wav payload assembled from TTS chunks");
    let wav = base64::engine::general_purpose::STANDARD
        .decode(audio)
        .unwrap();
    assert_eq!(&wav[0..4], b"RIFF", "payload is a WAV");
    // Two sentence chunks → two audio events in the seq list.
    let seqs: Vec<u64> = v["audio_seq"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![0, 1]);
}

#[tokio::test]
async fn wav_turn_transcribes_and_responds() {
    let (llm, tts, stt) = spawn_upstreams().await;
    let deps = deps_for(&llm, &tts, &stt);

    let resp = oneshot(
        deps,
        "POST",
        "/v1/turn",
        &[("content-type", "audio/wav")],
        Some(wav_body(&[500i16; 1600])), // 100 ms of tone
    )
    .await
    .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        TEST_TIMEOUT,
        axum::body::to_bytes(resp.into_body(), 1 << 20),
    )
    .await
    .expect("body read completes")
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["transcript"], "what time is it");
    assert_eq!(
        v["response_text"],
        "The quick brown fox jumps over the lazy dog. Then it wandered off to nap under the old oak tree."
    );
    assert!(v["audio_wav_base64"].as_str().unwrap().len() > 44);
}

#[tokio::test]
async fn wrong_api_key_is_401() {
    let (llm, tts, stt) = spawn_upstreams().await;
    let mut config = Config::default();
    config.server.api_keys = vec!["secret-key".into()];
    let deps = RouterDeps {
        config,
        llm: Arc::new(MockLlm { url: llm.uri() }),
        tts: Arc::new(MockTts { url: tts.uri() }),
        stt: Arc::new(MockStt { url: stt.uri() }),
        stt_realtime: None,
        plugins: Arc::new(PluginRegistry::new()),
        sessions: Arc::new(SessionStore::new()),
    };

    for headers in [
        vec![
            ("content-type", "application/json"),
            ("authorization", "Bearer wrong"),
        ],
        vec![("content-type", "application/json"), ("x-api-key", "wrong")],
    ] {
        let resp = oneshot(
            deps.clone(),
            "POST",
            "/v1/turn",
            &headers,
            Some(br#"{"text":"hi"}"#.to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // Correct key via either header shape → 200.
    for headers in [
        vec![
            ("content-type", "application/json"),
            ("authorization", "Bearer secret-key"),
        ],
        vec![
            ("content-type", "application/json"),
            ("x-api-key", "secret-key"),
        ],
    ] {
        let resp = oneshot(
            deps.clone(),
            "POST",
            "/v1/turn",
            &headers,
            Some(br#"{"text":"hi"}"#.to_vec()),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn garbage_content_type_is_415() {
    let (llm, tts, stt) = spawn_upstreams().await;
    let deps = deps_for(&llm, &tts, &stt);

    let resp = oneshot(
        deps,
        "POST",
        "/v1/turn",
        &[("content-type", "application/pdf")],
        Some(b"junk".to_vec()),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn healthz_reports_ok() {
    let (llm, tts, stt) = spawn_upstreams().await;
    let deps = deps_for(&llm, &tts, &stt);

    let resp = oneshot(deps, "GET", "/healthz", &[], None).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = tokio::time::timeout(TEST_TIMEOUT, axum::body::to_bytes(resp.into_body(), 1024))
        .await
        .expect("body read completes")
        .unwrap();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn missing_api_key_when_required_is_401() {
    let (llm, tts, stt) = spawn_upstreams().await;
    let mut config = Config::default();
    config.server.api_keys = vec!["secret-key".into()];
    let deps = RouterDeps {
        config,
        llm: Arc::new(MockLlm { url: llm.uri() }),
        tts: Arc::new(MockTts { url: tts.uri() }),
        stt: Arc::new(MockStt { url: stt.uri() }),
        stt_realtime: None,
        plugins: Arc::new(PluginRegistry::new()),
        sessions: Arc::new(SessionStore::new()),
    };

    let resp = oneshot(
        deps,
        "POST",
        "/v1/turn",
        &[("content-type", "application/json")],
        Some(br#"{"text":"hi"}"#.to_vec()),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wav_turn_with_no_speech_still_completes() {
    // Empty transcript returned by the STT mock → no LLM round, empty audio.
    let (llm, tts, _stt) = spawn_upstreams().await;
    let config = Config::default();
    // Point STT at a mock that returns an empty transcript.
    let empty_stt = MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "text": "" })))
        .mount(&empty_stt)
        .await;

    let deps = RouterDeps {
        config,
        llm: Arc::new(MockLlm { url: llm.uri() }),
        tts: Arc::new(MockTts { url: tts.uri() }),
        stt: Arc::new(MockStt {
            url: empty_stt.uri(),
        }),
        stt_realtime: None,
        plugins: Arc::new(PluginRegistry::new()),
        sessions: Arc::new(SessionStore::new()),
    };

    let resp = oneshot(
        deps,
        "POST",
        "/v1/turn",
        &[("content-type", "audio/wav")],
        Some(wav_body(&[5i16; 160])),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        TEST_TIMEOUT,
        axum::body::to_bytes(resp.into_body(), 1 << 20),
    )
    .await
    .expect("body read completes")
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["transcript"], "");
    assert_eq!(v["response_text"], "", "no LLM on empty transcript");
    assert_eq!(v["audio_wav_base64"], "", "no audio when the turn is empty");
}
