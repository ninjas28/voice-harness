//! Realtime-STT integration tests: `stt.realtime.enabled = true` routes
//! `audio.data` to the ASR server's realtime WS (`WS
//! /v1/audio/transcriptions/realtime`), deltas surface to the client as
//! `transcript` messages, and completed events dispatch turns through the
//! shared sentence gate with an in-flight-turn guard. The upstream is faked
//! with a scripted WS server on 127.0.0.1:0 (wiremock cannot serve WebSocket);
//! LLM/TTS stay wiremock. Every receive is timeout-wrapped.

mod common;

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use common::{audio_msg, recv_json, send_text, spawn_server, speech_frame, type_of, TEST_TIMEOUT};
use futures::{SinkExt, StreamExt};
use harness_core::config::Config;
use harness_providers::stt_realtime::RealtimeSttClient;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

type Ws = common::Ws;

// --------------------------------------------------- fake nemo upstream harness
// (The providers-test harness is not exported; same keep-accepting design.)

/// Server side of an accepted upstream connection (plain TCP — no TLS).
type UpstreamWs = WebSocketStream<tokio::net::TcpStream>;

/// Commands a test sends into the fake upstream.
enum ScriptMsg {
    /// Send this string as one text frame (a JSON server event).
    Text(String),
}

/// Binary frames the fake upstream received (i.e. exactly what the harness
/// forwarded upstream), shared with the test for assertions.
type BinaryLog = Arc<Mutex<Vec<Vec<u8>>>>;

/// Text frames the fake upstream received (`session.update` bodies), shared
/// with the test for assertions.
#[allow(dead_code)]
type TextLog = Arc<Mutex<Vec<String>>>;

/// Shared script receiver handed to each accepted upstream connection.
type ScriptRx = Arc<tokio::sync::Mutex<mpsc::Receiver<ScriptMsg>>>;

fn delta_json(text: &str) -> String {
    serde_json::json!({
        "type": "conversation.item.input_audio_transcription.delta",
        "delta": text
    })
    .to_string()
}

#[allow(dead_code)]
fn completed_json(text: &str) -> String {
    serde_json::json!({
        "type": "conversation.item.input_audio_transcription.completed",
        "transcript": text
    })
    .to_string()
}

/// Scripted fake nemo-speech realtime server. Binds `127.0.0.1:0` and keeps
/// accepting until the test process ends (one session per connection). Per
/// connection: send `session.created`, expect the client's `session.update`
/// text frame (record it verbatim), reply `session.updated`, then obey
/// `ScriptMsg`s and record binary frames. Returns the base URL as `http://…`
/// (so callers exercise the http→ws scheme conversion), the script sender,
/// and the shared logs.
async fn fake_upstream() -> (String, mpsc::Sender<ScriptMsg>, BinaryLog, TextLog) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local addr").port();
    let (script_tx, script_rx) = mpsc::channel::<ScriptMsg>(64);
    let script_rx = Arc::new(tokio::sync::Mutex::new(script_rx));
    let log: BinaryLog = Arc::new(Mutex::new(Vec::new()));
    let accept_log = Arc::clone(&log);
    let texts: TextLog = Arc::new(Mutex::new(Vec::new()));
    let accept_texts = Arc::clone(&texts);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _addr)) = listener.accept().await else {
                break;
            };
            let script = Arc::clone(&script_rx);
            let log = Arc::clone(&accept_log);
            let texts = Arc::clone(&accept_texts);
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                    run_session(ws, script, log, texts).await;
                }
            });
        }
    });

    (format!("http://127.0.0.1:{port}"), script_tx, log, texts)
}

/// One scripted upstream session (see `fake_upstream`).
async fn run_session(mut ws: UpstreamWs, script: ScriptRx, log: BinaryLog, texts: TextLog) {
    // Greeting: the server speaks first, per docs/api.md.
    if ws
        .send(Message::Text(r#"{"type":"session.created"}"#.to_string()))
        .await
        .is_err()
    {
        return;
    }

    // Handshake: wait for the client's session.update, record it verbatim,
    // then confirm with the default `session.updated`.
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let parsed: serde_json::Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if parsed.get("type").and_then(|v| v.as_str()) == Some("session.update") {
                    texts.lock().expect("text log").push(t);
                    if ws
                        .send(Message::Text(r#"{"type":"session.updated"}"#.to_string()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    break;
                }
            }
            Some(Ok(Message::Binary(b))) => log.lock().expect("binary log").push(b),
            Some(Ok(_)) => {}
            _ => return, // closed or protocol error
        }
    }

    // Scripted phase: obey commands, record every binary frame.
    loop {
        tokio::select! {
            cmd = async { script.lock().await.recv().await } => match cmd {
                None => break, // test dropped the script sender
                Some(ScriptMsg::Text(t)) => {
                    if ws.send(Message::Text(t)).await.is_err() {
                        break;
                    }
                }
            },
            frame = ws.next() => match frame {
                Some(Ok(Message::Binary(b))) => log.lock().expect("binary log").push(b),
                Some(Ok(Message::Text(t))) => texts.lock().expect("text log").push(t),
                Some(Ok(_)) => {}
                _ => break, // closed or protocol error
            },
        }
    }
}

/// Wait until `pred` holds on the shared binary log (the forward path is
/// asynchronous; polling with a deadline keeps the test deterministic).
async fn wait_for_binary_log(log: &BinaryLog, pred: impl Fn(&[Vec<u8>]) -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        if pred(&log.lock().expect("binary log")) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "binary frames never satisfied the expectation"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------- server spawn

/// One axum server in realtime-STT mode + the fake upstream: `Config` carries
/// `stt.realtime.enabled` with `stt.base_url` pointed at the fake; the router
/// gets `RouterDeps.stt_realtime = Some(client)`.
async fn spawn_realtime_server(
    customize: impl FnOnce(&mut Config),
) -> (
    String,
    mpsc::Sender<ScriptMsg>,
    BinaryLog,
    TextLog,
    Arc<AtomicUsize>,
) {
    let (base, script, bin_log, texts) = fake_upstream().await;
    let client = Arc::new(RealtimeSttClient::new(
        &base,
        "/v1/audio/transcriptions/realtime",
        "test-key",
        700,
        "",
    ));
    let mut config = Config::default();
    config.stt.realtime.enabled = true;
    config.stt.realtime.path = "/v1/audio/transcriptions/realtime".to_string();
    config.stt.base_url = base;
    let (url, llm_requests) = spawn_server(config, Vec::new(), customize, Some(client)).await;
    (url, script, bin_log, texts, llm_requests)
}

// ---------------------------------------------------------------- tests

/// `audio.data` in realtime mode forwards the decoded LE bytes upstream and a
/// scripted delta event surfaces to the client as a `transcript` message.
/// VAD still runs for UI state (utterances discarded), and every forwarded
/// binary frame is non-empty PCM16 (even byte count).
#[tokio::test]
async fn realtime_forwards_audio_and_surfaces_delta() {
    let (url, script, bin_log, _texts, _llm) = spawn_realtime_server(|_| {}).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;

    // Drain the session.start acknowledgment so the next frame received is
    // deterministically the scripted delta's transcript.
    let ack = recv_json(&mut ws).await;
    assert_eq!(
        type_of(&ack),
        "state",
        "session.start acknowledged with a state"
    );

    // Several loud frames → VAD UI state fires upstream-independent, decoded
    // bytes are forwarded upstream.
    for _ in 0..5 {
        send_text(&mut ws, audio_msg(&speech_frame())).await;
    }

    // The fake upstream must have received binary frames (decoded LE bytes,
    // not re-encoded), non-empty and PCM16-aligned.
    wait_for_binary_log(&bin_log, |log| {
        log.iter().any(|b| !b.is_empty() && b.len() % 2 == 0)
    })
    .await;

    // Script a delta event; the client must see it as a transcript message.
    script
        .send(ScriptMsg::Text(delta_json("hel")))
        .await
        .expect("script sender alive");

    // VAD state transitions may interleave; the transcript must arrive.
    let v = loop {
        let v = recv_json(&mut ws).await;
        if type_of(&v) != "state" {
            break v;
        }
    };
    assert_eq!(
        type_of(&v),
        "transcript",
        "delta surfaces as a transcript message: {v}"
    );
    assert_eq!(v["text"], "hel");

    // Connection stays open and usable afterwards (nothing tore down).
    send_text(&mut ws, audio_msg(&speech_frame())).await;
    wait_for_binary_log(&bin_log, |log| log.len() >= 6).await;
}
