//! Realtime WebSocket integration tests: a real axum server on 127.0.0.1:0
//! with wiremock upstreams. Full VAD turn, speech.end shortcut, malformed
//! input resilience. Every receive is timeout-wrapped.

mod common;

use std::sync::atomic::Ordering;

use common::{
    audio_msg, recv_json, send_text, silence_frame, spawn_server, spawn_server_with_keys,
    speech_frame, type_of,
};
use futures::StreamExt;
use harness_core::config::Config;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::Message;

type Ws = common::Ws;

/// Speech then silence, in frames (30 ms each).
async fn speak_then_pause(ws: &mut Ws, speech_frames: usize, silence_frames: usize) {
    for _ in 0..speech_frames {
        send_text(ws, audio_msg(&speech_frame())).await;
    }
    for _ in 0..silence_frames {
        send_text(ws, audio_msg(&silence_frame())).await;
    }
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

    // A token in the query string is REFUSED: query strings leak into
    // proxies/access logs, so auth is header-only. (The ?token= fallback
    // was removed for exactly that reason.)
    let result = tokio_tungstenite::connect_async(format!("{url}?token=secret")).await;
    assert!(result.is_err(), "query-string token is refused");

    // Authorization header works.
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

// ------------------------------------------------- sentence-end transcript gate

/// A VAD-finalized utterance whose transcript lacks terminal punctuation is
/// transcribed and shown to the client, but no turn starts and the LLM is
/// not called (long wait so the flush timer cannot fire within the window).
#[tokio::test]
async fn nonterminal_transcript_is_held_no_llm_call() {
    let (url, llm_requests) = spawn_server(
        Config::default(),
        vec!["can you look up uh".to_string()],
        |cfg| cfg.session.sentence_end_wait_ms = 60_000,
        None,
        None,
    )
    .await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;

    // ~0.6 s speech + ~1 s silence → endpoint fires, STT runs, gate holds.
    speak_then_pause(&mut ws, 20, 33).await;

    // Well past any plausible dispatch latency: nothing may arrive.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                assert_ne!(
                    type_of(&v),
                    "turn.completed",
                    "held fragment must not dispatch: {v}"
                );
            }
            _ => break,
        }
    }
    assert_eq!(
        llm_requests.load(Ordering::SeqCst),
        0,
        "nonterminal fragment must not reach the LLM"
    );
}

/// The next utterance's transcript is concatenated onto the held fragment and
/// the whole sentence dispatches as ONE turn when it ends terminal.
#[tokio::test]
async fn continuation_concatenates_and_dispatches_once() {
    let (url, llm_requests) = spawn_server(
        Config::default(),
        vec![
            "can you look up uh".to_string(),
            "the weather for tomorrow?".to_string(),
        ],
        |_| {},
        None,
        None,
    )
    .await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;

    // Fragment 1: speech + silence → held (client sees its echo).
    speak_then_pause(&mut ws, 20, 33).await;
    // Fragment 2: arrives before the wait timer (2 s) elapses.
    speak_then_pause(&mut ws, 20, 33).await;

    let mut saw_transcript = false;
    let mut saw_completed = false;
    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "transcript" => {
                // Two transcript echoes arrive (the fragment's own echo, then
                // the concatenated final); only the final one must be whole.
                if v["text"] == "can you look up uh the weather for tomorrow?" {
                    saw_transcript = true;
                }
            }
            "turn.completed" => {
                assert!(!saw_completed, "only one turn for both fragments");
                saw_completed = true;
                break;
            }
            "error" => panic!("unexpected error frame: {v}"),
            _ => {}
        }
    }
    assert!(
        saw_transcript && saw_completed,
        "single concatenated turn (final transcript must precede completion)"
    );
    assert_eq!(
        llm_requests.load(Ordering::SeqCst),
        1,
        "exactly one LLM round for the two fragments"
    );
}

/// A held fragment dispatches anyway after `sentence_end_wait_ms` of silence,
/// so a complete command without punctuation can never hang the session.
#[tokio::test]
async fn held_fragment_dispatches_after_wait_timeout() {
    let (url, llm_requests) = spawn_server(
        Config::default(),
        vec!["can you look up the weather".to_string()],
        |cfg| {
            cfg.session.sentence_end_wait_ms = 500;
        },
        None,
        None,
    )
    .await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;

    speak_then_pause(&mut ws, 20, 33).await;

    // Endpoint fires → hold → 500 ms later the wait dispatches the turn.
    let start = tokio::time::Instant::now();
    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "turn.completed" => break,
            "error" => panic!("unexpected error frame: {v}"),
            _ => {}
        }
    }
    assert!(
        start.elapsed() >= Duration::from_millis(450),
        "dispatch happened suspiciously early — fragment may not have been held"
    );
    assert_eq!(llm_requests.load(Ordering::SeqCst), 1);
}

/// An explicit `speech.end` with no open utterance flushes a held fragment
/// immediately — the client says "that's all", no timer wait.
#[tokio::test]
async fn speech_end_flushes_held_fragment() {
    let (url, llm_requests) = spawn_server(
        Config::default(),
        vec!["can you look up the weather".to_string()],
        |cfg| cfg.session.sentence_end_wait_ms = 60_000,
        None,
        None,
    )
    .await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;

    speak_then_pause(&mut ws, 20, 33).await;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "speech.end" }).to_string(),
    )
    .await;

    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "turn.completed" => break,
            "error" => panic!("unexpected error frame: {v}"),
            _ => {}
        }
    }
    assert_eq!(llm_requests.load(Ordering::SeqCst), 1);
}

/// The flush deadline is deferred while the VAD has an utterance open: a long
/// continuation must not be cut off by the timer mid-speech. If the timer
/// fired during the speech run, the two fragments would dispatch separately
/// (2 LLM calls); deferral keeps it at exactly one joined turn.
#[tokio::test]
async fn wait_timer_defers_while_speech_is_open() {
    let (url, llm_requests) = spawn_server(
        Config::default(),
        vec![
            "can you look up uh".to_string(),
            "the weather for tomorrow?".to_string(),
        ],
        |cfg| {
            cfg.session.sentence_end_wait_ms = 500;
        },
        None,
        None,
    )
    .await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;

    // Fragment 1 held; 500 ms timer armed.
    speak_then_pause(&mut ws, 20, 33).await;
    // 1.5 s of continuous speech (> 500 ms): timer must defer throughout.
    for _ in 0..50 {
        send_text(&mut ws, audio_msg(&speech_frame())).await;
    }
    // Then silence endpoints utterance 2 → joined dispatch.
    for _ in 0..33 {
        send_text(&mut ws, audio_msg(&silence_frame())).await;
    }

    let mut saw_joined_transcript = false;
    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "transcript" => {
                if v["text"] == "can you look up uh the weather for tomorrow?" {
                    saw_joined_transcript = true;
                }
            }
            "turn.completed" => break,
            "error" => panic!("unexpected error frame: {v}"),
            _ => {}
        }
    }
    assert!(
        saw_joined_transcript,
        "deferral must produce one joined turn"
    );
    assert_eq!(
        llm_requests.load(Ordering::SeqCst),
        1,
        "timer firing mid-speech would dispatch the fragments separately"
    );
}
