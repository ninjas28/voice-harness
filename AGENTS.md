# AGENTS.md

Guidance for AI coding agents working in this repository. Read this before
making changes; it encodes conventions and hard-won lessons from this codebase.

## Project

**voice-harness** is a Rust daemon bridging audio clients (ESP32-class smart
speakers, the bundled macOS menu-bar client) to self-hosted voice services:

- **STT/TTS**: any OpenAI-compatible voice server (`/v1/audio/*` routes)
- **LLM**: any OpenAI-compatible chat endpoint (e.g. Open WebUI) — note the
  client configures `base_url + chat_path`; don't double-prefix `/api`

The harness packages a transcribed turn with a system prompt and MCP-style
plugins, calls the LLM, streams the response back as text deltas and
sentence-chunked TTS audio.

## Layout

```
crates/
  harness-core/      config, wire types, VAD, utterance assembly, chunker
  harness-providers/ STT/TTS/LLM HTTP clients (wiremock-tested, never live)
  harness-plugins/   tool-call loop + built-in plugins (time, weather, web_search) + MCP client/plugin (Streamable HTTP, bearer/OAuth)
  harness-server/    axum HTTP (`POST /v1/turn`) + WS (`/v1/realtime`)
examples/loopback.rs WAV file → full turn over WS (manual E2E check)
clients/macos/voice-harness-client/  SwiftPM macOS menu-bar client
clients/ios/voice-harness-client/    iOS app (XcodeGen project; shares voicekit via path dep)
config/voice-harness.toml             server config (API keys — never commit)
.hermes/plans/                        task plans (markdown)
```

## Wire protocol (dotted tags)

Client → server: `session.start` (sample_rate, default 16000), `audio.data`
(base64 PCM16 16 kHz mono), `speech.end`, `session.stop`.
Server → client: `state` (listening|speech|thinking|speaking), `transcript`,
`response.text.delta`, `audio.chunk` (base64 PCM16 16 kHz mono, seq from 0),
`turn.completed`, `error`.

## Non-negotiable workflow

- **Strict TDD**: write the failing test first, watch it fail (RED), then make
  it pass (GREEN). Commit per task. Small, focused commits
  (`fix:`, `feat:`, `test:`, `docs:`, `refactor:`).
- **Gates before every commit** (all must pass):
  ```
  cargo fmt --all
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```
  Swift (in `clients/macos/voice-harness-client/`):
  ```
  swift build && swift test
  ```
- **Never probe real servers from tests or agents.** Providers are tested
  against `wiremock` only. Real endpoints are verified manually, deliberately.
- **Secrets**: `config/voice-harness.toml` holds API keys. Never echo, log, or
  commit them. `config/voice-harness.toml.example` is the committed template.
- **Bounded everything**: every async stream/receive/collect in tests must be
  wrapped in `tokio::time::timeout` (10 s in Rust, ≤2 s waits in Swift player
  tests). No unbounded streams, no retry loops without a cap. An unbounded
  collect in this repo once OOM'd an entire machine.
- **Wiremock gotchas**: a responder closure receives `&Request` (not `Request`
  — closure type mismatches are cryptic E0631s) and there is no built-in
  request counter. When a mock provider trait already wraps the upstream URL
  (e.g. `MockLlm { url, requests }`), count there — incrementing the same
  `Arc<AtomicUsize>` from both the wiremock closure AND the trait mock
  double-counts every call.
- **Tokio time races with default config**: a test that sleeps past a config
  default (`sentence_end_wait_ms = 2s`, future `silence_ms`-adjacent timers)
  hits the production timer. Tests must set wait knobs explicitly — long
  (60s) to prove a hold, short (500ms) to prove a flush.
- Commits are local-only (no push) unless explicitly asked.

## Hard-won domain lessons

- **WebRTC VAD accepts only 10/20/30 ms frames** (≤480 samples @ 16 kHz).
  Anything else classifies as silence. The server must split arbitrary client
  chunks into ≤480-sample sub-frames (`ConnState::VAD_FRAME_SAMPLES`) before
  feeding the VAD/assembler — do not "fix" this on the client side; ESP32
  clients will also send odd framings.
- **One `AVAudioEngine` per process** on macOS, especially with Bluetooth
  headsets: two engines fight over the route (A2DP vs HFP) and the playback
  engine's buffers "complete" without ever sounding. Share the engine between
  `AudioPlayer` and `MicCapture`.
- **Install input taps before `engine.start()`** — a tap added to a running
  engine's input node silently never fires.
- **`AVAudioUnitTimePitch` (and effect units generally) reject Int16
  connections** (error -10868). Convert to canonical Float32 buffers; keep the
  wire protocol PCM16.
- **The server cannot know when the client finishes playing audio.** While
  client-side playback is pending, the client owns the phase and must absorb
  server `state` messages — including `speech` (the mic hears the client's own
  TTS; there is no AEC). The drain watcher completes the transition.
- **URLSessionWebSocketTask needs `task.resume()`** after creation, or
  `send()` suspends forever.
- **`MenuBarExtra(.window)` freezes its window at first-measured height**;
  `.fixedSize(horizontal: false, vertical: true)` lets the panel grow with
  content.
- **LSEnvironment / `open --env` don't reach unsigned GUI bundles**; use
  `launchctl setenv VAR value` when a launch-time env var is genuinely needed.
- **Pass the config path bare** (`harness-server config/voice-harness.toml`);
  there is no `--config` flag.
- **AppIntents under Swift 6 strict concurrency**: `@MainActor` goes on
  `perform()` only, never on the intent struct — a struct-isolated
  conformance fails with "conformance crosses into main actor-isolated code".
- **Pin the OS in simulator destinations** (`platform=iOS Simulator,name=iPhone 16 Pro,OS=18.5`):
  a bare `name=` resolves to OS=latest, and installed devices exist only on
  specific runtimes, so the unpinned form fails destination lookup.
- **ControlWidget intents must live in the app target too**: `openAppWhenRun`
  only takes effect when the AppIntent type is compiled into the app (not
  just the widget extension) — otherwise the control tap runs in the
  extension process and the app never opens. Share the intent file across
  both targets and verify the app's `Metadata.appintents/extract.actionsdata`
  lists the intent.
- **nemo-speech realtime transcription events** (verified from
  `server/http/http_server.cpp`, not the docs): a delta event carries the
  append increment in field `delta` (the full revised partial when the new one
  doesn't extend the previous), a completed event the full text in field
  `transcript`, and an error event nests its message under `error.message`.
  The harness accumulates deltas by appending.

## Realtime upstream STT (nemo-speech.cpp)

Two STT modes behind `[stt.realtime].enabled` (default off = batch, unchanged):
batch is harness VAD → finalized utterance → multipart WAV POST; realtime
streams client audio frames to the ASR server's own
`/v1/audio/transcriptions/realtime` WebSocket and receives partial/final
transcripts. Client-visible wire protocol is identical in both modes — clients
need zero changes.

- `harness-providers/src/stt_realtime.rs` owns the upstream: `RealtimeSttClient`
  (dial + `session.created` → `session.update` → `session.updated` handshake,
  10 s bound) and `spawn_link` (bounded 64/64 mpsc pump; `SttRealtimeCmd::
  Pcm/Commit/Clear` in, `Result<SttRealtimeEvent, _>` out; link dropped = pump
  exits silently). No tungstenite types leak through the link API.
- `ws.rs` runs it as `SttMode::Realtime` per connection: lazy connect on first
  `audio.data` (dial failure → `error{stt}`, retry next chunk), raw decoded LE
  bytes forwarded unchanged, delta increments accumulated server-side and
  surfaced as the cumulative `transcript` partial (clients replace their line —
  forwarding raw increments would flash word-by-word), completed events feed
  the shared sentence gate (`gate_transcript`), `speech.end` sends
  `input_audio_buffer.commit`, session start/stop/teardown drop the link and
  the next chunk reconnects. `dispatch_turn` guards against a second turn while
  one is in flight (protects both modes).
- WebRTC VAD still runs in realtime mode — **for UI state only** (instant
  `listening`/`speech` transitions); the upstream's `endpointing_ms` owns
  utterance segmentation. There are no `speech_started`/`speech_stopped`
  events in the transcription protocol (those are VoiceChat-only).
- Tests fake the upstream with a scripted `TcpListener` +
  `tokio_tungstenite::accept_async` server (wiremock cannot serve WebSocket) —
  see `harness-providers/tests/stt_realtime.rs` and the copy in
  `harness-server/tests/ws_realtime_stt.rs`.

## MCP servers (Streamable HTTP)

- Tools from `[plugins.mcp]` servers surface as `mcp.<server>.<tool>`; the single
  `mcp` plugin aggregates all servers and warms at startup (`PluginRegistry::warm_all`).
- A dead/unauthorized MCP server never blocks boot — it is skipped with a warning and
  contributes no tools.
- Auth: `auth = "bearer"` + `api_key`, or `auth = "oauth"` (OAuth 2.1, PKCE, dynamic
  client registration, loopback redirect). Authorize once with
  `harness-server auth <name>`; tokens live in `config/mcp-tokens.json` (chmod 600,
  never commit). Refresh is automatic; a hard 401 tells the LLM to surface the error.
- The MCP client is hand-rolled (`crates/harness-plugins/src/mcp/`), wiremock-tested,
  and timeout-bounded everywhere — no MCP SDK dependency.
- Session handling: `Mcp-Session-Id` is tracked per server; a 404 re-initializes once
  and retries. Only `initialize` / `tools/list` / `tools/call` are spoken.

## Client deployment (macOS)

```
cd clients/macos/voice-harness-client
swift build
pkill -f VoiceHarnessClient.app
cp .build/debug/VoiceHarnessClient /Applications/VoiceHarnessClient.app/Contents/MacOS/VoiceHarnessClient
open /Applications/VoiceHarnessClient.app
```

The app is launchd-owned via `open`; bare background launches get reaped.
Settings persist in the app's `UserDefaults` domain (`server_url`, `tts_rate`).

## Style

- Rust: workspace crates, `anyhow`-free typed errors (`thiserror`), no `unwrap`
  in server paths, `tracing` for logs.
- Swift: zero-dependency SwiftPM, Swift 6 strict concurrency, actors for
  transport, `@MainActor` for UI state, injectable `Transport` protocol for
  tests.
- Keep plugin tools async and cancellable; the orchestrator aborts turns.
