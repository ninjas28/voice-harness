//! Loopback client: streams a WAV file to the server's realtime WebSocket and
//! prints every server message. Audio playback is out of scope — this exists
//! to exercise the full STT → LLM → TTS pipeline end to end from a terminal.
//!
//! Usage:
//!
//! ```text
//! cargo run -p harness-server --example loopback -- --ws ws://127.0.0.1:8090/v1/realtime --file test.wav [--token KEY]
//! ```
//!
//! The file is read as WAV (any sample rate; mono/stereo is downmixed by the
//! providers' WAV parser), resampled to 16 kHz, and streamed as ~30 ms PCM16
//! frames — the same shape an ESP32 client would produce live.

use base64::Engine as _;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use harness_providers::tts::{resample_to_16k, wav_to_pcm16};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Frame duration the wire protocol expects (matches the server's VAD).
const FRAME_MS: u32 = 30;
const SAMPLE_RATE: u32 = 16_000;

#[derive(Parser)]
struct Args {
    /// Realtime endpoint, e.g. ws://127.0.0.1:8090/v1/realtime
    #[arg(long)]
    ws: String,
    /// WAV file to stream as a single utterance
    #[arg(long)]
    file: std::path::PathBuf,
    /// API key (sent as an `Authorization: Bearer` header; omit when the
    /// server requires none)
    #[arg(long)]
    token: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Load + decode the WAV, resample to 16 kHz mono.
    let bytes = std::fs::read(&args.file).unwrap_or_else(|e| {
        eprintln!("cannot read {}: {e}", args.file.display());
        std::process::exit(1);
    });
    let (samples, rate) = wav_to_pcm16(&bytes).unwrap_or_else(|e| {
        eprintln!("not a WAV file: {e}");
        std::process::exit(1);
    });
    let pcm = resample_to_16k(&samples, rate);
    println!(
        "loaded {}: {} samples @ {rate} Hz → {} @ {SAMPLE_RATE} Hz ({:.1} s)",
        args.file.display(),
        samples.len(),
        pcm.len(),
        pcm.len() as f64 / f64::from(SAMPLE_RATE),
    );

    // Connect (key in the Authorization header — query strings leak into
    // proxies/access logs).
    let mut request = args
        .ws
        .clone()
        .into_client_request()
        .expect("valid websocket url");
    if let Some(token) = &args.token {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    let (mut ws, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .expect("server reachable");

    // Open a session.
    ws.send(Message::text(
        serde_json::json!({ "type": "session.start", "device_id": "loopback-cli" }).to_string(),
    ))
    .await
    .expect("send session.start");

    // Stream the utterance in ~30 ms frames (480 samples @ 16 kHz).
    let frame_len = (SAMPLE_RATE / 1000 * FRAME_MS) as usize;
    for chunk in pcm.chunks(frame_len) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(
            chunk
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        ws.send(Message::text(
            serde_json::json!({ "type": "audio.data", "pcm": b64 }).to_string(),
        ))
        .await
        .expect("send audio.data");
        // Pace roughly like live mic audio so server-side VAD sees real timing.
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(FRAME_MS))).await;
    }

    // Force the endpoint: no trailing silence needed.
    ws.send(Message::text(
        serde_json::json!({ "type": "speech.end" }).to_string(),
    ))
    .await
    .expect("send speech.end");

    // Print everything the server says until turn.completed (then stop).
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(30), ws.next())
            .await
            .expect("server reply within 30 s")
            .expect("connection open")
            .expect("no ws error");
        let Message::Text(text) = msg else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(&text).expect("server json");
        match v["type"].as_str().unwrap_or("?") {
            "state" => println!("[state] {}", v["state"].as_str().unwrap_or("?")),
            "transcript" => println!("[transcript] {}", v["text"].as_str().unwrap_or("")),
            "response.text.delta" => print!("{}", v["text"].as_str().unwrap_or("")),
            "response.text" => println!("\n[response] {}", v["text"].as_str().unwrap_or("")),
            "audio.chunk" => println!(
                "[audio.chunk seq={}] {} samples (not played)",
                v["seq"],
                v["pcm"].as_str().map(|p| p.len() * 3 / 4 / 2).unwrap_or(0)
            ),
            "turn.completed" => {
                println!("[turn.completed] done");
                break;
            }
            "error" => {
                println!(
                    "[error code={}] {}",
                    v["code"].as_str().unwrap_or("?"),
                    v["message"].as_str().unwrap_or("")
                );
                std::process::exit(1);
            }
            other => println!("[{other}] {v}"),
        }
    }

    // Clean shutdown.
    let _ = ws
        .send(Message::text(
            serde_json::json!({ "type": "session.stop" }).to_string(),
        ))
        .await;
    let _ = ws.close(None).await;
}
