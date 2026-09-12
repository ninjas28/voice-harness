# voice-harness

Rust voice-assistant bridge: sits between clients (ESP32-class smart speakers, desktop apps)
and the existing voice servers (Nemotron ASR + magpie TTS on voicebox, Open WebUI LLM on ai).
Accepts streamed PCM16 mic audio or pre-transcribed text, runs an agentic pipeline
(system prompt + plugin tools -> LLM), returns transcript + sentence-chunked TTS audio.

## Layout

- `crates/harness-core` — config, wire protocol types, VAD/utterance FSM, chunker
- `crates/harness-providers` — STT / TTS / LLM upstream clients
- `crates/harness-plugins` — plugin trait, registry, built-in plugins
- `crates/harness-server` — axum app: `POST /v1/turn`, `WS /v1/realtime`, loopback example
- `config/voice-harness.toml.example` — config template (real config is gitignored)

## Quickstart

### 1. Build

```sh
cargo build --release -p harness-server
```

### 2. Configure

```sh
cp config/voice-harness.toml.example config/voice-harness.toml
# fill in the three SET-ME api keys, or export them instead:
#   VH_STT__API_KEY=... VH_TTS__API_KEY=... VH_LLM__API_KEY=...
```

`Config::load` reads `config/voice-harness.toml` by default (a path may be passed as
the first CLI argument). Env vars use the `VH_<SECTION>__<KEY>` form and win over
the file.

### 3. Run

```sh
cargo run -p harness-server
# → voice-harness listening on http://127.0.0.1:8090
curl http://127.0.0.1:8090/healthz   # → ok
```

### 4. Try a turn over HTTP

Text turn:

```sh
curl -s http://127.0.0.1:8090/v1/turn \
  -H 'content-type: application/json' \
  -d '{"text": "what time is it", "device_id": "kitchen"}'
# → {"transcript":"what time is it","response_text":"...","audio_wav_base64":"...","audio_seq":2}
```

Or post a WAV directly (16 kHz mono PCM16 recommended; other rates are resampled):

```sh
curl -s http://127.0.0.1:8090/v1/turn \
  -H 'content-type: audio/wav' \
  --data-binary @test.wav
```

### 5. Try the realtime WebSocket

```sh
cargo run -p harness-server --example loopback -- \
  --ws ws://127.0.0.1:8090/v1/realtime --file test.wav
```

The loopback example loads any WAV, resamples to 16 kHz, streams it as ~30 ms
PCM16 frames over a websocket, and prints every server message. Audio playback
is out of scope; point an ESP32-class client at the same endpoint for the real thing.

## Auth

If `server.api_keys` is non-empty, every request (HTTP and the WS handshake) must
present a key via `Authorization: Bearer <key>`, `X-API-Key: <key>`, or — for
websocket clients that cannot set headers — `?token=<key>` in the URL.
Empty `api_keys` (the default) means no auth: intended for localhost/LAN trust.

## Wire protocol (WS `/v1/realtime`)

JSON text frames, one object per frame. Client → server:

| type | fields | meaning |
|---|---|---|
| `session.start` | `device_id?`, `sample_rate?` | open a session (aborts any in-flight turn) |
| `audio.data` | `pcm` (base64 PCM16, any length) | feed the VAD; server segments utterances |
| `speech.end` | — | force the current utterance to end now |
| `session.stop` | — | close the session (server disconnects) |

Server → client:

| type | fields | meaning |
|---|---|---|
| `state` | `state` | `listening` → `speech` → `thinking` → `speaking` → `listening` |
| `transcript` | `text` | utterance transcribed |
| `response.text.delta` | `text` | streamed LLM tokens |
| `response.text` | `text` | full reply text |
| `audio.chunk` | `pcm` (base64), `seq` | sentence-chunked TTS audio, `seq` starts at 0 |
| `turn.completed` | — | the turn is finished |
| `error` | `code`, `message` | protocol or pipeline failure; connection stays open |

Endpointing is server-side VAD (WebRTC VAD, aggressive) with the `[session]`
policy: `silence_ms` of trailing silence ends an utterance, `min_utterance_ms`
drops blips, `max_utterance_ms` caps duration, `pre_speech_ms` of audio is
included from the ring buffer. `session.start` / `session.stop` abort an
in-flight turn — the v1 interruption story.

## Status

Phase A complete: workspace scaffold, config loading, wire protocol types,
VAD/utterance FSM, orchestrator (text + audio turns), HTTP turn API with auth,
realtime WebSocket sessions with server-side VAD, loopback client example.
