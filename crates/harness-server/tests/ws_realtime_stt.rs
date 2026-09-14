//! Realtime-STT integration tests: `stt.realtime.enabled = true` routes
//! `audio.data` to the ASR server's realtime WS (`WS
//! /v1/audio/transcriptions/realtime`), deltas surface to the client as
//! `transcript` messages, and completed events dispatch turns through the
//! shared sentence gate with an in-flight-turn guard. The upstream is faked
//! with a scripted WS server on 127.0.0.1:0 (wiremock cannot serve WebSocket);
//! LLM/TTS stay wiremock. Every receive is timeout-wrapped.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use common::{
    audio_msg, recv_json, send_text, silence_frame, spawn_server, speech_frame, type_of,
    TEST_TIMEOUT,
};
use futures::{SinkExt, StreamExt};
use harness_core::config::Config;
use harness_providers::stt_realtime::RealtimeSttClient;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

type Ws = common::Ws;

/// Short bounded wait for lifecycle expectations (teardown, reconnect).
/// Localhost operations are sub-second; 2 s gives slack while staying well
/// inside the pump's 10 s idle read timeout, so a passing test cannot be
/// riding the idle-timeout link death.
const BOUNDED_WAIT: Duration = Duration::from_secs(2);

// --------------------------------------------------- fake nemo upstream harness
// (The providers-test harness is not exported; same keep-accepting design.)

/// Server side of an accepted upstream connection (plain TCP — no TLS).
type UpstreamWs = WebSocketStream<tokio::net::TcpStream>;

/// Commands a test sends into the fake upstream.
enum ScriptMsg {
    /// Send this string as one text frame (a JSON server event).
    Text(String),
    /// Close the current scripted session's socket — simulates the upstream
    /// dying mid-session (pump death on the harness side).
    Close,
}

/// Binary frames the fake upstream received (i.e. exactly what the harness
/// forwarded upstream), shared with the test for assertions.
type BinaryLog = Arc<Mutex<Vec<Vec<u8>>>>;

/// Text frames the fake upstream received (`session.update` bodies), shared
/// with the test for assertions.
type TextLog = Arc<Mutex<Vec<String>>>;

/// Accept/end counts for the fake upstream: how many WS sessions were
/// accepted, and how many of those ended with a closed/errored socket. The
/// end count is how a test proves the harness actually dropped its link
/// (the upstream sees EOF) rather than just going quiet.
#[derive(Default, Clone)]
struct UpstreamLog {
    accepts: Arc<AtomicUsize>,
    ends: Arc<AtomicUsize>,
}

impl UpstreamLog {
    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }
    fn ends(&self) -> usize {
        self.ends.load(Ordering::SeqCst)
    }
}

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
/// `ScriptMsg`s and record binary/text frames. Returns the base URL as
/// `http://…` (so callers exercise the http→ws scheme conversion), the script
/// sender, the shared logs, and accept/end counters.
async fn fake_upstream() -> (
    String,
    mpsc::Sender<ScriptMsg>,
    BinaryLog,
    TextLog,
    UpstreamLog,
) {
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
    let counts = UpstreamLog::default();

    let (accepts, ends) = (Arc::clone(&counts.accepts), Arc::clone(&counts.ends));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _addr)) = listener.accept().await else {
                break;
            };
            let script = Arc::clone(&script_rx);
            let log = Arc::clone(&accept_log);
            let texts = Arc::clone(&accept_texts);
            accepts.fetch_add(1, Ordering::SeqCst);
            let ends = Arc::clone(&ends);
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                    run_session(ws, script, log, texts).await;
                }
                // This accepted session is over — cleanly or via error.
                ends.fetch_add(1, Ordering::SeqCst);
            });
        }
    });

    (
        format!("http://127.0.0.1:{port}"),
        script_tx,
        log,
        texts,
        counts,
    )
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

    // Scripted phase: obey commands, record every binary/text frame.
    loop {
        tokio::select! {
            cmd = async { script.lock().await.recv().await } => match cmd {
                None => break, // test dropped the script sender
                Some(ScriptMsg::Text(t)) => {
                    if ws.send(Message::Text(t)).await.is_err() {
                        break;
                    }
                }
                Some(ScriptMsg::Close) => break, // simulate upstream death
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
        if Instant::now() >= deadline {
            let got = log.lock().expect("binary log").len();
            panic!("binary frames never satisfied the expectation (log has {got} frames)");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until `pred` holds on the shared text log (frames the harness sent
/// upstream after the handshake, e.g. `input_audio_buffer.commit`).
async fn wait_for_text_log(log: &TextLog, pred: impl Fn(&[String]) -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        if pred(&log.lock().expect("text log")) {
            return;
        }
        if Instant::now() >= deadline {
            let got = log.lock().expect("text log").join(" | ");
            panic!("upstream text frames never satisfied the expectation (log: [{got}])");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until the accept/end counters satisfy `pred` (reconnect tests).
/// `limit` bounds the wait tightly: localhost reconnects/teardowns are
/// sub-second, and the production pump's idle read timeout is 10 s — a short
/// bound here proves fast teardown/reconnect instead of accidentally riding
/// the idle-timeout death.
async fn wait_for_counts_within(
    counts: &UpstreamLog,
    pred: impl Fn(&UpstreamLog) -> bool,
    limit: Duration,
) {
    let deadline = Instant::now() + limit;
    loop {
        if pred(counts) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "upstream accept/end counters never satisfied the expectation within {limit:?} \
                 (accepts={}, ends={})",
                counts.accepts(),
                counts.ends()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------- server spawn

/// One axum server in realtime-STT mode + the fake upstream: `Config` carries
/// `stt.realtime.enabled` with `stt.base_url` pointed at the fake; the router
/// gets `RouterDeps.stt_realtime = Some(client)`.
#[allow(clippy::type_complexity)]
async fn spawn_realtime_server(
    customize: impl FnOnce(&mut Config),
) -> (
    String,
    mpsc::Sender<ScriptMsg>,
    BinaryLog,
    TextLog,
    UpstreamLog,
    Arc<AtomicUsize>,
) {
    let (base, script, bin_log, texts, counts) = fake_upstream().await;
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
    (url, script, bin_log, texts, counts, llm_requests)
}

// ---------------------------------------------------------------- tests

/// `audio.data` in realtime mode forwards the decoded LE bytes upstream and a
/// scripted delta event surfaces to the client as a `transcript` message.
/// VAD still runs for UI state (utterances discarded), and every forwarded
/// binary frame is non-empty PCM16 (even byte count).
#[tokio::test]
async fn realtime_forwards_audio_and_surfaces_delta() {
    let (url, script, bin_log, _texts, _counts, _llm) = spawn_realtime_server(|_| {}).await;
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

// ---------------------------------------------------------------- Task 9

/// A sentence-terminal `completed` event surfaces the transcript, then
/// dispatches the turn through the shared sentence gate: the client sees
/// transcript → state:thinking → response.text.delta → audio.chunk →
/// turn.completed, and the LLM is hit exactly once.
#[tokio::test]
async fn completed_dispatches_turn_through_sentence_gate() {
    let (url, script, _bin_log, _texts, _counts, llm_requests) =
        spawn_realtime_server(|_| {}).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(type_of(&ack), "state", "session.start ack drains first");

    // Some audio upstream (link live), then a sentence-terminal final.
    send_text(&mut ws, audio_msg(&speech_frame())).await;
    script
        .send(ScriptMsg::Text(completed_json("what time is it?")))
        .await
        .expect("script sender alive");

    let mut saw_transcript = false;
    let mut saw_thinking = false;
    let mut saw_delta = false;
    let mut saw_chunk = false;
    let mut saw_completed = false;
    loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "transcript" => {
                saw_transcript = true;
                assert_eq!(v["text"], "what time is it?");
            }
            "state" => {
                if v["state"].as_str().unwrap_or("?") == "thinking" {
                    saw_thinking = true;
                }
            }
            "response.text.delta" => saw_delta = true,
            "audio.chunk" => saw_chunk = true,
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
        saw_transcript && saw_thinking && saw_delta && saw_chunk && saw_completed,
        "completed dispatches the full turn pipeline"
    );
    assert_eq!(
        llm_requests.load(Ordering::SeqCst),
        1,
        "exactly one LLM round"
    );
}

/// A second `completed` while the first turn's task is still running must not
/// start a second turn: still exactly one `turn.completed` and one LLM hit.
#[tokio::test]
async fn second_completed_while_turn_in_flight_is_dropped() {
    let (url, script, _bin_log, _texts, _counts, llm_requests) =
        spawn_realtime_server(|_| {}).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(type_of(&ack), "state", "session.start ack drains first");

    send_text(&mut ws, audio_msg(&speech_frame())).await;
    // Two finals back-to-back: the second lands while turn 1 is in flight.
    script
        .send(ScriptMsg::Text(completed_json("what time is it?")))
        .await
        .expect("script sender alive");
    script
        .send(ScriptMsg::Text(completed_json("and the weather?")))
        .await
        .expect("script sender alive");

    let mut saw_transcripts = 0;
    let mut completed_count = 0;
    // Collect well past the first turn's completion.
    let deadline = Instant::now() + TEST_TIMEOUT;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(1500), recv_json(&mut ws)).await {
            Ok(v) => match type_of(&v) {
                "transcript" => saw_transcripts += 1,
                "turn.completed" => {
                    completed_count += 1;
                    if completed_count == 1 {
                        // First turn done; keep listening to catch any
                        // unauthorized second turn.
                        continue;
                    }
                }
                "error" => panic!("unexpected error frame: {v}"),
                _ => {}
            },
            Err(_) => break, // quiet window: no more events coming
        }
    }
    assert!(
        saw_transcripts >= 2,
        "both finals echo as transcripts: {saw_transcripts}"
    );
    assert_eq!(
        completed_count, 1,
        "in-flight guard must suppress the second turn"
    );
    assert_eq!(
        llm_requests.load(Ordering::SeqCst),
        1,
        "guard must keep the LLM at one hit"
    );
}

// ---------------------------------------------------------------- Task 10

/// `speech.end` with speech flowing flushes the upstream buffer: the fake
/// upstream must receive an `input_audio_buffer.commit` text frame.
#[tokio::test]
async fn speech_end_sends_commit_upstream() {
    let (url, _script, _bin_log, texts, _counts, _llm) = spawn_realtime_server(|_| {}).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(type_of(&ack), "state", "session.start ack drains first");

    // Speech flowing (utterance open on the UI VAD), then an explicit end.
    for _ in 0..10 {
        send_text(&mut ws, audio_msg(&speech_frame())).await;
    }
    send_text(
        &mut ws,
        serde_json::json!({ "type": "speech.end" }).to_string(),
    )
    .await;

    // The upstream must see exactly the flush command as a text frame.
    wait_for_text_log(&texts, |log| {
        log.iter()
            .any(|t| t.contains("\"input_audio_buffer.commit\""))
    })
    .await;
}

/// `speech.end` with a held fragment and NO utterance open flushes it
/// immediately: the held "can you look up the weather" dispatches as a turn
/// (thinking → … → turn.completed) instead of waiting out the 60 s timer.
#[tokio::test]
async fn speech_end_flushes_held_fragment_without_open_utterance() {
    let (url, script, _bin_log, _texts, _counts, llm_requests) =
        spawn_realtime_server(|cfg| cfg.session.sentence_end_wait_ms = 60_000).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(type_of(&ack), "state", "session.start ack drains first");

    // Fragment 1 endpointed upstream → non-terminal → held (60 s deadline).
    send_text(&mut ws, audio_msg(&speech_frame())).await;
    script
        .send(ScriptMsg::Text(completed_json(
            "can you look up the weather",
        )))
        .await
        .expect("script sender alive");
    // UI state transitions (speech/listening) may interleave; the fragment
    // echo must arrive.
    let held = loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "transcript" => break v,
            "state" => {}
            _ => panic!("expected the held fragment's transcript echo: {v}"),
        }
    };
    assert_eq!(held["text"], "can you look up the weather");

    // Utterance closed upstream (endpoint already fired): silence so the UI
    // VAD closes too, then "that's all" → flush now.
    for _ in 0..30 {
        send_text(&mut ws, audio_msg(&silence_frame())).await;
    }
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
    assert_eq!(
        llm_requests.load(Ordering::SeqCst),
        1,
        "held fragment must reach the LLM exactly once"
    );
}

/// A fresh `session.start` on a live connection tears the upstream link down
/// (the fake upstream sees its socket close), and the next audio.data lazily
/// reconnects — the fake upstream's accept count reaches 2. `session.stop`
/// then ends the connection AND the current upstream link (the upstream sees
/// EOF again). (A bare `session.stop` disconnects the client WS by design —
/// the v1 interruption story — so the reconnect proof rides on
/// `session.start`, which runs the same teardown.)
#[tokio::test]
async fn session_stop_drops_upstream_and_next_session_reconnects() {
    let (url, _script, _bin_log, _texts, counts, _llm) = spawn_realtime_server(|_| {}).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(type_of(&ack), "state", "session.start ack drains first");

    // Link comes up on the first chunk (accept #1).
    send_text(&mut ws, audio_msg(&speech_frame())).await;
    wait_for_counts_within(&counts, |c| c.accepts() >= 1, BOUNDED_WAIT).await;

    // Fresh session.start: the upstream must see EOF quickly (teardown) —
    // well inside the pump's 10 s idle timeout, which must NOT be what ends
    // the link here.
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(type_of(&ack), "state", "second session.start ack");
    wait_for_counts_within(&counts, |c| c.ends() >= 1, BOUNDED_WAIT).await;

    // The next audio chunk reconnects (accept #2).
    send_text(&mut ws, audio_msg(&speech_frame())).await;
    wait_for_counts_within(&counts, |c| c.accepts() >= 2, BOUNDED_WAIT).await;

    // session.stop: the connection ends AND the current upstream link drops.
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.stop" }).to_string(),
    )
    .await;
    wait_for_counts_within(&counts, |c| c.ends() >= 2, BOUNDED_WAIT).await;
}

/// Pump death mid-session (the fake upstream closes its socket): the harness
/// surfaces an upstream-closed Error{code:"stt"}, tears the link down, and
/// the next audio.data spawns a fresh link (second accept) instead of
/// erroring forever.
#[tokio::test]
async fn pump_death_reconnects_on_next_audio_chunk() {
    let (url, script, bin_log, _texts, counts, _llm) = spawn_realtime_server(|_| {}).await;
    let mut ws: Ws = tokio_tungstenite::connect_async(&url).await.unwrap().0;

    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(type_of(&ack), "state", "session.start ack drains first");

    // Link up (accept #1), bytes flowing.
    send_text(&mut ws, audio_msg(&speech_frame())).await;
    wait_for_binary_log(&bin_log, |log| !log.is_empty()).await;

    // Kill the upstream socket: the pump dies, the harness must see the
    // upstream-closed error and tear the link down. UI state transitions may
    // interleave before it.
    script
        .send(ScriptMsg::Close)
        .await
        .expect("script sender alive");
    let err = loop {
        let v = recv_json(&mut ws).await;
        match type_of(&v) {
            "error" => break v,
            "state" => {}
            _ => panic!("expected the upstream-closed error: {v}"),
        }
    };
    assert_eq!(err["code"], "stt", "error code identifies the STT path");
    wait_for_counts_within(&counts, |c| c.ends() >= 1, BOUNDED_WAIT).await;

    // Next chunk reconnects lazily instead of erroring forever (accept #2)
    // and forwards to the NEW upstream session. The byte-count snapshot is
    // taken BEFORE the reconnect chunk so the new session's frame is what
    // the final wait observes.
    let before = bin_log.lock().expect("binary log").len();
    send_text(&mut ws, audio_msg(&speech_frame())).await;
    wait_for_counts_within(&counts, |c| c.accepts() >= 2, BOUNDED_WAIT).await;
    wait_for_binary_log(&bin_log, |log| log.len() > before).await;
}
