# voice-harness

A Rust daemon that bridges audio clients — ESP32-class smart speakers, the
bundled macOS menu-bar client — to self-hosted voice services:

```
mic audio ──▶ harness ──▶ STT (nemo-speech.cpp @ voicebox.zippystation.com)
                 │
                 ├─▶ LLM (Open WebUI @ ai.zippystation.com/api, model glm53)
                 │      └─ custom system prompt + MCP-style tool plugins
                 │
                 └─▶ TTS (voicebox) ──▶ sentence-chunked audio back to client
```

- **Server-side VAD** with utterance endpointing: clients just stream mic
  frames; the harness decides when an utterance ends and a turn begins.
- **Two surfaces**: `POST /v1/turn` (JSON: pre-transcribed text or base64 audio
  in, transcript + response + WAV audio out) and `WS /v1/realtime`
  (streaming audio in, streaming text + audio chunks out).
- **Plugin/tool-call loop**: MCP-style plugins answer LLM tool calls (a `time`
  tool ships by default).
- **macOS menu-bar client** (`clients/macos/voice-harness-client`): live
  transcript, phase indicator (listening/speech/thinking/speaking), and
  pitch-preserving TTS speed control.

## Building

Requires Rust (stable, edition 2024).

```sh
cargo build --release
cp config/voice-harness.toml.example config/voice-harness.toml
$EDITOR config/voice-harness.toml   # fill in API keys + endpoints
./target/release/harness-server config/voice-harness.toml
curl http://127.0.0.1:8090/healthz  # → ok
```

The macOS client:

```sh
cd clients/macos/voice-harness-client
swift build
.build/debug/VoiceHarnessClient
```

## Configuration

`config/voice-harness.toml` (see `voice-harness.toml.example`):

- `[server]` — bind address, `api_keys` (clients must present one as a bearer
  token), `sample_rate` (16 kHz)
- `[stt]` / `[tts]` — voicebox base URL + API key
- `[llm]` — Open WebUI `base_url` **plus** `chat_path`
  (`base_url = "https://ai.zippystation.com/api"`,
  `chat_path = "/v1/chat/completions"` — the two are concatenated; don't
  double-prefix), `model`, `system_prompt`

## Wire protocol (`/v1/realtime`)

JSON text frames with dotted `type` tags; audio is base64 PCM16, 16 kHz mono.

| Direction | Type | Payload |
|---|---|---|
| C→S | `session.start` | `device_id?`, `sample_rate?` (default 16000) |
| C→S | `audio.data` | `pcm` (base64 PCM16); arbitrary framing is fine |
| C→S | `speech.end` | optional; server-side VAD also endpointed |
| C→S | `session.stop` | ends the session |
| S→C | `state` | `listening` / `speech` / `thinking` / `speaking` |
| S→C | `transcript` | STT result for the utterance |
| S→C | `response.text.delta` | streamed LLM text |
| S→C | `audio.chunk` | `pcm` + `seq` (from 0), one TTS sentence per chunk |
| S→C | `turn.completed` | turn finished server-side |
| S→C | `error` | `code` + `message` |

`POST /v1/turn` accepts `{"text": "..."}` or `{"audio_base64": "..."}`
(WAV/PCM16) and returns transcript, response text, and full audio in one JSON
body. All endpoints require `Authorization: Bearer <api_key>`.

## Tests

```sh
cargo test --workspace          # Rust (wiremock-only; never hits real servers)
cargo clippy --workspace --all-targets -- -D warnings
cd clients/macos/voice-harness-client && swift test
```

Manual end-to-end check (needs a running server + keys):

```sh
cargo run --release --example loopback -- \
  --ws ws://127.0.0.1:8090/v1/realtime --file /path/to/speech.wav
```

## Client notes (macOS)

- The panel is a `MenuBarExtra` window; settings persist in the
  `com.zippystation.voice-harness-client` defaults domain (`server_url`,
  `tts_rate`).
- Mic and playback share one `AVAudioEngine` — required for Bluetooth headsets
  (two engines fight over the route and playback goes silent).
- Deploy as an app bundle under `/Applications` and launch with `open`
  (launchd-owned); bare background launches get reaped.

## Status

- Harness: complete — 90 Rust tests green (fmt + clippy clean), verified
  end-to-end against the real STT/TTS/LLM servers (text turn, audio turn, WS
  streaming with server-side VAD).
- macOS client: complete — 20 Swift tests green; live-verified: mic streaming,
  server VAD turns, streamed TTS playback with adjustable speed, phase
  tracking owned by the client during playback.
- Deferred (by design): mic-paused-while-speaking (no barge-in without AEC),
  push-to-talk. Server deployment uses the systemd unit in `deploy/`.
