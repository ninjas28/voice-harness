//! Tests for the realtime STT provider against a fake nemo-speech upstream
//! (`WS /v1/audio/transcriptions/realtime`; project-specific events — NOT
//! OpenAI Realtime, NOT VoiceChat). wiremock cannot serve WebSocket, so the
//! upstream is a raw `TcpListener` + `tokio_tungstenite::accept_async` bound
//! to 127.0.0.1:0 only — never a real server.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Server side of an accepted upstream connection (plain TCP — no TLS).
type Ws = WebSocketStream<tokio::net::TcpStream>;

/// Commands a test sends into the fake upstream.
enum ScriptMsg {
    /// Send this string as one text frame (a JSON server event).
    Text(String),
    /// Marker for "a binary frame is expected now". Binary frames are always
    /// recorded, so this only pins the test's intent/ordering.
    AssertBinary,
}

/// Binary frames the fake upstream received (i.e. exactly what the code under
/// test forwarded upstream), shared with the test for assertions.
type BinaryLog = Arc<Mutex<Vec<Vec<u8>>>>;

/// Scripted fake nemo-speech realtime server. Binds `127.0.0.1:0` and keeps
/// accepting until the test process ends (one session per connection, so
/// reconnect tests can count accepts). Per connection: send
/// `session.created`, expect the client's `session.update` text frame, reply
/// `session.updated`, then loop obeying `ScriptMsg`s and recording binary
/// frames. Returns the base URL as `http://…` (so callers exercise the
/// http→ws scheme conversion), the script sender, and the shared binary log.
async fn fake_upstream() -> (String, mpsc::Sender<ScriptMsg>, BinaryLog) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local addr").port();
    let (script_tx, script_rx) = mpsc::channel::<ScriptMsg>(64);
    let script_rx = Arc::new(tokio::sync::Mutex::new(script_rx));
    let log: BinaryLog = Arc::new(Mutex::new(Vec::new()));
    let accept_log = Arc::clone(&log);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _addr)) = listener.accept().await else {
                break;
            };
            let script = Arc::clone(&script_rx);
            let log = Arc::clone(&accept_log);
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                    run_session(ws, script, log).await;
                }
            });
        }
    });

    (format!("http://127.0.0.1:{port}"), script_tx, log)
}

/// One scripted upstream session (see `fake_upstream`).
async fn run_session(
    mut ws: Ws,
    script: Arc<tokio::sync::Mutex<mpsc::Receiver<ScriptMsg>>>,
    log: BinaryLog,
) {
    // Greeting: the server speaks first, per docs/api.md.
    if ws
        .send(Message::Text(r#"{"type":"session.created"}"#.to_string()))
        .await
        .is_err()
    {
        return;
    }

    // Handshake: wait for the client's session.update, then confirm. Match on
    // the parsed `type`, not a text prefix — serde_json serializes objects
    // with sorted keys, so `type` is never first in the wire bytes.
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let parsed: serde_json::Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if parsed.get("type").and_then(|v| v.as_str()) == Some("session.update") {
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
                Some(ScriptMsg::AssertBinary) => {}
            },
            frame = ws.next() => match frame {
                Some(Ok(Message::Binary(b))) => log.lock().expect("binary log").push(b),
                Some(Ok(_)) => {}
                _ => break, // closed or protocol error
            },
        }
    }
}

/// Smoke test for the harness itself (its verification): the scripted
/// `session.created` greeting arrives, the session.update handshake is
/// answered, binary frames land in the shared log verbatim, and scripted
/// text events reach the client — all under the 10 s bound.
#[tokio::test]
async fn fake_upstream_serves_scripted_session_and_records_binary() {
    let (base, script, recorded) = fake_upstream().await;
    let ws_url = base.replacen("http://", "ws://", 1);
    let (mut ws, _resp) = timeout(TIMEOUT, tokio_tungstenite::connect_async(&ws_url))
        .await
        .expect("connect within timeout")
        .expect("connect ok");

    // Scripted greeting arrives first, verbatim.
    let greeting = timeout(TIMEOUT, ws.next())
        .await
        .expect("greeting within timeout")
        .expect("frame")
        .expect("no ws error");
    match greeting {
        Message::Text(t) => assert_eq!(t, r#"{"type":"session.created"}"#),
        other => panic!("expected text greeting, got {other:?}"),
    }

    // Handshake: client sends session.update, fake replies session.updated.
    let update = serde_json::json!({"type": "session.update", "session": {"sample_rate": 16000}});
    timeout(TIMEOUT, ws.send(Message::Text(update.to_string())))
        .await
        .expect("send within timeout")
        .expect("send ok");
    let reply = timeout(TIMEOUT, ws.next())
        .await
        .expect("reply within timeout")
        .expect("frame")
        .expect("no ws error");
    match reply {
        Message::Text(t) => assert!(t.contains(r#""type":"session.updated""#), "reply: {t}"),
        other => panic!("expected text reply, got {other:?}"),
    }

    // Binary frames are recorded verbatim in the shared log.
    timeout(TIMEOUT, ws.send(Message::Binary(vec![1, 2, 3, 4])))
        .await
        .expect("send within timeout")
        .expect("send ok");
    script
        .send(ScriptMsg::AssertBinary)
        .await
        .expect("script sender alive");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if recorded
            .lock()
            .expect("binary log")
            .last()
            .map(Vec::as_slice)
            == Some(&[1u8, 2, 3, 4][..])
        {
            break;
        }
        assert!(Instant::now() < deadline, "binary frame never recorded");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Scripted text events reach the client.
    let delta = r#"{"type":"conversation.item.input_audio_transcription.delta","delta":"hel"}"#;
    script
        .send(ScriptMsg::Text(delta.to_string()))
        .await
        .expect("script sender alive");
    let frame = timeout(TIMEOUT, ws.next())
        .await
        .expect("event within timeout")
        .expect("frame")
        .expect("no ws error");
    match frame {
        Message::Text(t) => assert_eq!(t, delta),
        other => panic!("expected scripted text event, got {other:?}"),
    }
}
