# voice-harness

A Rust daemon that bridges audio clients — ESP32-class smart speakers, the
bundled macOS menu-bar client — to self-hosted voice services:

```
mic audio ──▶ harness ──▶ STT (any OpenAI-compatible voice server)
                 │
                 ├─▶ LLM (any OpenAI-compatible chat endpoint, e.g. Open WebUI)
                 │      └─ custom system prompt + tool plugins (incl. MCP)
                 │
                 └─▶ TTS (same voice server) ──▶ sentence-chunked audio back to client
```

- **Server-side VAD** with utterance endpointing: clients just stream mic
  frames; the harness decides when an utterance ends and a turn begins. When
  the STT server is nemo-speech.cpp, `[stt.realtime]` switches to streaming
  mode instead: mic frames are forwarded to the ASR server's realtime
  WebSocket, transcripts arrive as partials while you speak, and the LLM turn
  dispatches ~`endpointing_ms` after you stop (see below).
- **Two surfaces**: `POST /v1/turn` (JSON: pre-transcribed text or base64 audio
  in, transcript + response + WAV audio out) and `WS /v1/realtime`
  (streaming audio in, streaming text + audio chunks out).
- **Plugin/tool-call loop**: tool plugins answer LLM tool calls (a `time`
  tool and a keyless `weather` tool ship by default; external MCP servers plug
  in via `[plugins.mcp]`).
- **macOS menu-bar client** (`clients/macos/voice-harness-client`): live
  transcript, phase indicator (listening/speech/thinking/speaking), and
  pitch-preserving TTS speed control.

### MCP tools

The harness can call tools on external MCP servers (Streamable HTTP). Configure them
under `[plugins.mcp]` in `config/voice-harness.toml`; see `voice-harness.toml.example`.
Bearer-key and OAuth 2.1 servers are supported (`harness-server auth <name>` performs
the OAuth browser flow once; token refresh is automatic).

## Building

Requires Rust (stable, edition 2021).

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

- `[server]` — bind address, `api_keys` (clients must present one via an
  `Authorization: Bearer` or `X-API-Key` header — auth is header-only;
  query-string tokens are refused), `sample_rate` (16 kHz)
- `[stt]` / `[tts]` — voice-server base URL + API key
- `[stt.realtime]` — optional streaming STT over the ASR server's realtime
  transcription WebSocket (nemo-speech.cpp): `enabled` (default `false` —
  batch mode stays the fallback), `path`
  (`/v1/audio/transcriptions/realtime`), `endpointing_ms` (end-of-utterance
  silence threshold, default 700), `language` (empty = model default).
  Reuses `[stt]`'s `base_url`/`api_key` (same server).
- `[llm]` — chat endpoint `base_url` **plus** `chat_path` (the two are
  concatenated; don't double-prefix — for Open WebUI, `base_url` ends in
  `/api` and `chat_path` starts with `/v1/`), `model`, `reasoning_effort`

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

### Sentence-end gate

VAD endpointing fires on any pause longer than `session.silence_ms`, so a
mid-sentence hesitation ("can you look up uh … the weather for tomorrow?")
would otherwise become several separate LLM turns. With
`session.require_sentence_end = true` (the default), the realtime session
instead holds a transcribed utterance that does not end in sentence-terminal
punctuation (`. ! ? …`) and concatenates it with the next utterance's
transcript; one turn dispatches when the joined transcript finally ends a
sentence. Safety valves: a held fragment dispatches anyway after
`session.sentence_end_wait_ms` of silence with no continuation (the deadline
is deferred while the VAD has an utterance open), and an explicit `speech.end`
flushes it immediately. Transcripts are echoed to the client either way, so
the panel shows fragments as they are recognized.

### Streaming STT (server-side VAD)

With `[stt.realtime].enabled = true` and a nemo-speech.cpp ASR server, the
harness stops segmenting and transcribing utterances itself: each client
`audio.data` chunk is forwarded raw to the server's
`/v1/audio/transcriptions/realtime` WebSocket, and the server's own
endpointing (`endpointing_ms`) decides where utterances end. Benefits over
batch mode:

- **Live partials** — `transcript` messages arrive while you are still
  speaking (each is the running partial, not yet the final text).
- **Lower turn latency** — the LLM starts ~`endpointing_ms` after you stop
  talking, instead of `session.silence_ms` + WAV upload + full-utterance
  inference.
- The WebRTC VAD still runs, but only to flip the client's
  `listening`/`speech` state instantly; finals dispatch through the same
  sentence-end gate as batch mode (see below), so `session.require_sentence_end`
  behavior is unchanged.

Reconnection is automatic and lazy: if the upstream WebSocket dies, the next
audio chunk reconnects. `speech.end` still works — it flushes the upstream
buffer (`input_audio_buffer.commit`) plus any held transcript fragment.

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

- The panel is a `MenuBarExtra` window; the gear button opens an in-panel
  server URL editor. Saving validates (`ws://`/`wss://` + host), persists to
  the app's `UserDefaults` domain (`server_url`, `tts_rate`), and stops a
  running session — the next Start reconnects to the new endpoint. Clearing
  the field restores the default (`ws://127.0.0.1:8090/v1/realtime`).
- Mic and playback share one `AVAudioEngine` — required for Bluetooth headsets
  (two engines fight over the route and playback goes silent).
- Deploy as an app bundle under `/Applications` and launch with `open`
  (launchd-owned); bare background launches get reaped.

## Client notes (iOS)

- `clients/ios/voice-harness-client` is an XcodeGen project: run
  `xcodegen generate` (from that directory), then build with
  `xcodebuild -project VoiceHarnessApp.xcodeproj -scheme VoiceHarnessApp
  -destination 'platform=iOS Simulator,name=iPhone 16 Pro,OS=18.5' build`.
- It consumes the same `voicekit` package as the macOS client (SwiftPM path
  dependency; the package exposes it as a library product). Protocol,
  transport, audio, settings, and session-model code is single-sourced, and
  the same test suite runs on macOS (`swift test`) and the iOS simulator
  (`xcodebuild test`).
- "Toggle Voice Harness" App Shortcut: assign via Settings → Action Button →
  Shortcut, add to Home Screen from Shortcuts (ⓘ → Add to Home Screen), or
  Back Tap (Settings → Accessibility → Touch → Back Tap).
- `UIBackgroundModes: audio` keeps the session alive when the phone locks;
  the first Start triggers the iOS mic permission prompt.

## Status

- Harness: complete — 184 Rust tests green (fmt + clippy clean); realtime
  upstream STT (nemo-speech.cpp `[stt.realtime]`) implemented and
  fake-upstream-tested; pending: live verification against the real ASR
  server's realtime endpoint. Earlier: verified end-to-end against the real
  STT/TTS/LLM servers (text turn, audio turn, WS streaming with server-side
  VAD).
- macOS client: complete — 30 Swift tests green; live-verified: mic streaming,
  server VAD turns, streamed TTS playback with adjustable speed, phase
  tracking owned by the client during playback, in-panel server URL editor.
- iOS client: complete (simulator) — shares the `voicekit` package with macOS
  (30 tests green on the iOS simulator too); "Toggle Voice Harness" App
  Shortcut for Action Button / Home Screen / Back Tap; launches and renders
  the panel on the simulator. Pending: live E2E voice check against the real
  server (human-assisted) and real-device build (needs signing).
- Deferred (by design): mic-paused-while-speaking (no barge-in without AEC),
  push-to-talk. Server deployment uses the systemd unit in `deploy/`.
