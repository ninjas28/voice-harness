//! Integration tests for the TTS client (wiremock upstream).

use harness_providers::tts::{OpenAiTtsClient, TtsProvider};

fn client_for(server: &wiremock::MockServer) -> OpenAiTtsClient {
    OpenAiTtsClient::new(
        server.uri(),
        "/v1/audio/speech",
        "tts-key",
        "tts-model",
        "default",
        "wav",
    )
}

fn client_for_raw(server: &wiremock::MockServer, raw_rate: u32) -> OpenAiTtsClient {
    OpenAiTtsClient::with_raw_sample_rate(
        server.uri(),
        "/v1/audio/speech",
        "tts-key",
        "tts-model",
        "default",
        "pcm",
        raw_rate,
    )
}

/// Build a minimal 44-byte-header WAV with the given rate and 16-bit samples.
fn wav_bytes(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let mut w = Vec::new();
    let data_len = samples.len() * 2;
    let bits = 16u16;
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    let byte_rate = rate * channels as u32 * bits as u32 / 8;
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&(channels * bits / 8).to_le_bytes()); // block align
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

async fn collect(
    p: impl std::future::Future<Output = Result<Vec<u8>, harness_core::error::HarnessError>>,
) -> Vec<u8> {
    // Hard bound so a hung request fails in seconds, not never.
    tokio::time::timeout(std::time::Duration::from_secs(10), p)
        .await
        .expect("synthesize completed within 10s")
        .expect("synthesize ok")
}

#[tokio::test]
async fn wav_response_is_parsed_to_16k_pcm16() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/audio/speech"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer tts-key",
        ))
        .and(wiremock::matchers::body_partial_json(serde_json::json!({
            "model": "tts-model",
            "voice": "default",
            "input": "hello there",
            "response_format": "wav"
        })))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_raw(wav_bytes(16_000, 1, &[100, -200, 300, -400]), "audio/wav"),
        )
        .mount(&server)
        .await;

    let client = client_for(&server);
    let pcm = collect(client.synthesize("hello there")).await;
    let expected: Vec<u8> = [100i16, -200, 300, -400]
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    assert_eq!(pcm, expected);
}

#[tokio::test]
async fn raw_24k_pcm_is_resampled_to_16k() {
    let server = wiremock::MockServer::start().await;
    // 2400 samples of a 240 Hz sine at 24 kHz — 10 full cycles.
    let sine: Vec<i16> = (0..2400)
        .map(|i| (2400.0 * (2.0 * std::f64::consts::PI * 240.0 * i as f64 / 24_000.0).sin()) as i16)
        .collect();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_raw(
                    sine.iter()
                        .flat_map(|s| s.to_le_bytes())
                        .collect::<Vec<u8>>(),
                    "application/octet-stream",
                ),
        )
        .mount(&server)
        .await;

    let client = client_for_raw(&server, 24_000);
    let pcm = collect(client.synthesize("raw body")).await;
    let out: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    // resampled length = in_len * 16000/24000 = 1600
    assert_eq!(out.len(), 1600, "resample must scale length by 2/3");
}

#[tokio::test]
async fn upstream_error_maps_to_harness_error() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("tts exploded"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.synthesize("anything"),
    )
    .await
    .expect("completed within 10s")
    .expect_err("must be an error");
    let msg = err.to_string();
    assert!(msg.contains("tts"), "unexpected: {msg}");
    assert!(msg.contains("500"), "unexpected: {msg}");
}

/// Regression: a hung upstream (connection up, body never arrives) used to
/// hang `synthesize` forever — the reqwest client only set `connect_timeout`,
/// so a stalled TTS body read froze the turn task, the end-of-turn
/// `turn.completed` burst never went out, and clients stuck in `speaking`
/// (see the `completionWatchdog` lesson in AGENTS.md). The client must carry
/// a request timeout so the turn errors out instead of hanging.
#[tokio::test]
async fn hung_tts_body_times_out_instead_of_hanging() {
    use std::time::Duration;

    let server = wiremock::MockServer::start().await;
    // 4 s delay: well past the client's request timeout, well inside the
    // test's own 10 s hard bound.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_raw(wav_bytes(16_000, 1, &[1, 2, 3]), "audio/wav")
                .set_delay(std::time::Duration::from_secs(4)),
        )
        .mount(&server)
        .await;

    // Short knob: request timeout well inside the mock's 4s delay and the
    // 10s test bound — the client must error before the (slow but alive)
    // mock ever answers.
    let client = OpenAiTtsClient::with_raw_sample_rate_and_timeout(
        server.uri(),
        "/v1/audio/speech",
        "tts-key",
        "tts-model",
        "default",
        "wav",
        16_000,
        Duration::from_millis(500),
    );
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.synthesize("hello there"),
    )
    .await
    .expect("synthesize returned within the 10s test bound (request timeout must fire first)")
    .expect_err("hung upstream must error, not hang");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "request timeout must fire before the mock's 4s delay elapsed, took {:?}",
        started.elapsed()
    );
    let msg = err.to_string();
    assert!(msg.contains("tts"), "unexpected: {msg}");
}

#[tokio::test]
async fn from_config_uses_tts_section() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_raw(wav_bytes(16_000, 1, &[7, 8, 9]), "audio/wav"),
        )
        .mount(&server)
        .await;

    let cfg = harness_core::config::Config::default().tts;
    let client = OpenAiTtsClient::from_config(&harness_core::config::TtsConfig {
        base_url: server.uri(),
        speech_path: "/v1/audio/speech".into(),
        api_key: "cfg-key".into(),
        model: "tts-model".into(),
        voice: "default".into(),
        response_format: "wav".into(),
        ..cfg
    });
    let pcm = collect(client.synthesize("cfg test")).await;
    assert_eq!(pcm.len(), 6);
}
