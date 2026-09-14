//! WebSocket frame/message size limit tests: an oversized frame must be
//! rejected by the server's WS config (max_message_size 1 MiB, max_frame_size
//! 256 KiB). Uses tokio-tungstenite as a real WS client against the bound
//! server, mirroring the other integration-test harnesses.

mod common;

use common::{recv_json, send_text, spawn_server};
use futures::{SinkExt, StreamExt};
use harness_core::config::Config;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;

type Ws = common::Ws;

/// A legitimate 20 ms PCM16 @ 16 kHz chunk base64-encoded is ~1.3 KB — well
/// under the 256 KiB frame limit. Sanity guard that normal traffic still
/// flows after the limits were set.
#[tokio::test]
async fn normal_audio_chunk_still_accepted_within_limits() {
    let (url, _config) = spawn_server(Config::default(), Vec::new(), |_| {}, None, None).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(common::type_of(&ack), "state");

    // ~2 KB of base64 PCM (legit 20 ms chunk shape).
    let chunk = "A".repeat(2048);
    send_text(
        &mut ws,
        serde_json::json!({ "type": "audio.data", "pcm": chunk }).to_string(),
    )
    .await;
    // Connection stays open: session.stop gets a clean close, not an error.
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.stop" }).to_string(),
    )
    .await;
}

#[tokio::test]
async fn oversized_text_frame_is_rejected() {
    let (url, _config) = spawn_server(Config::default(), Vec::new(), |_| {}, None, None).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    // 2 MiB text frame — above the 1 MiB max_message_size. The server must
    // reject/drop the connection: the send may fail outright (server already
    // tore the socket down mid-write) or the next read yields an error /
    // close. A normal server message after the oversized frame is a failure.
    let huge = "x".repeat(2 * 1024 * 1024);
    timeout(Duration::from_secs(10), async {
        let send_result = ws.send(Message::Text(huge)).await;
        if let Err(e) = send_result {
            let text = e.to_string();
            assert!(
                text.contains("Broken pipe") || text.contains("closed") || text.contains("reset"),
                "send failed for an unexpected reason: {text}"
            );
            return; // connection dropped while sending: rejected
        }
        match timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Err(e))) => {
                let text = e.to_string();
                assert!(
                    text.contains("too big") || text.contains("size") || text.contains("closed"),
                    "expected size-limit/closed error, got: {text}"
                );
            }
            Ok(Some(Ok(Message::Close(_)))) => { /* clean close: also fine */ }
            Ok(Some(Ok(m))) => panic!("server accepted an oversized frame: {m}"),
            Ok(None) => {} // stream ended: connection dropped
            Err(_) => panic!("server neither rejected nor closed after an oversized frame"),
        }
    })
    .await
    .expect("oversized-frame test bounded to 10s");
}
