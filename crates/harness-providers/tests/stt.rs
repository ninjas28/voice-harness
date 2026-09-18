//! Integration tests for the STT client (wiremock upstream).
//!
//! Note: wiremock's `body_string_contains` matcher runs a strict UTF-8
//! conversion, which can never match a multipart body that embeds binary
//! WAV bytes — so the multipart contract is asserted by inspecting
//! `received_requests()` bodies (lossy-decoded) instead of matchers.

use harness_providers::stt::{OpenAiSttClient, SttProvider};

fn client_for(server: &wiremock::MockServer) -> OpenAiSttClient {
    OpenAiSttClient::new(
        server.uri(),
        "/v1/audio/transcriptions",
        "stt-key",
        "asr-model",
    )
}

async fn collect(
    p: impl std::future::Future<Output = Result<String, harness_core::error::HarnessError>>,
) -> String {
    // Hard bound so a hung request fails in seconds, not never.
    tokio::time::timeout(std::time::Duration::from_secs(10), p)
        .await
        .expect("transcribe completed within 10s")
        .expect("transcribe ok")
}

#[tokio::test]
async fn multipart_upload_parses_transcript_response() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/audio/transcriptions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer stt-key",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "text": "what time is it" })),
        )
        .mount(&server)
        .await;

    let client = client_for(&server);
    let pcm = vec![100i16, -200, 300];
    let text = collect(client.transcribe(&pcm)).await;
    assert_eq!(text, "what time is it");

    // Verify the multipart contract from what the mock actually received.
    let reqs = server.received_requests().await.expect("received requests");
    assert_eq!(reqs.len(), 1);
    let body = String::from_utf8_lossy(&reqs[0].body);
    assert!(body.contains("RIFF"), "must upload a WAV file: {body}");
    assert!(body.contains("name=\"file\""), "file part: {body}");
    assert!(body.contains("filename=\"audio.wav\""), "file name: {body}");
    assert!(body.contains("name=\"model\""), "model field: {body}");
    assert!(body.contains("asr-model"), "model value: {body}");
    let ct = reqs[0]
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(ct.starts_with("multipart/form-data"), "ct: {ct}");
}

#[tokio::test]
async fn empty_audio_short_circuits_without_network_call() {
    let server = wiremock::MockServer::start().await;
    // No mocks mounted: any request would 404 and fail the test.
    let client = client_for(&server);
    let text = collect(client.transcribe(&[0i16; 4096])).await;
    assert_eq!(text, "");
    // A near-silent blip is also skipped.
    let text = collect(client.transcribe(&[1i16, -1, 0, 2])).await;
    assert_eq!(text, "");
    assert!(
        server
            .received_requests()
            .await
            .expect("received requests")
            .is_empty(),
        "no network call may happen for silent audio"
    );
}

#[tokio::test]
async fn accepts_verbose_json_shape() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "task": "transcribe",
                "language": "en",
                "duration": 1.5,
                "text": "hello world"
            })),
        )
        .mount(&server)
        .await;

    let client = client_for(&server);
    let pcm = vec![500i16; 100];
    let text = collect(client.transcribe(&pcm)).await;
    assert_eq!(text, "hello world");
}

#[tokio::test]
async fn upstream_error_maps_to_harness_error() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.transcribe(&[500i16; 100]),
    )
    .await
    .expect("completed within 10s")
    .expect_err("must be an error");
    let msg = err.to_string();
    assert!(msg.contains("stt"), "unexpected: {msg}");
    assert!(msg.contains("401"), "unexpected: {msg}");
}

/// Regression: a hung upstream (connection up, body never arrives) used to
/// hang `transcribe` forever — the reqwest client only set `connect_timeout`,
/// so a stalled STT body read froze the turn task and the end-of-turn burst
/// never went out (see the stalled-turn lesson in AGENTS.md). The client must
/// carry a request timeout so the turn errors out instead of hanging.
#[tokio::test]
async fn hung_stt_body_times_out_instead_of_hanging() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "text": "late" }))
                .set_delay(std::time::Duration::from_secs(4)),
        )
        .mount(&server)
        .await;

    // Short knob: request timeout well inside the mock's 4s delay and the
    // 10s test bound — the client must error before the (slow but alive)
    // mock ever answers.
    let client = OpenAiSttClient::new_with_timeout(
        server.uri(),
        "/v1/audio/transcriptions",
        "stt-key",
        "asr-model",
        std::time::Duration::from_millis(500),
    );
    let pcm = vec![500i16; 100];
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(std::time::Duration::from_secs(10), client.transcribe(&pcm))
        .await
        .expect("transcribe returned within the 10s test bound (request timeout must fire first)")
        .expect_err("hung upstream must error, not hang");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "request timeout must fire before the mock's 4s delay elapsed, took {:?}",
        started.elapsed()
    );
    let msg = err.to_string();
    assert!(msg.contains("stt"), "unexpected: {msg}");
}
