# voice-harness server deployment

Target: a Linux box running the harness in front of any OpenAI-compatible
voice server (STT/TTS) and chat endpoint (LLM), e.g. Open WebUI.

## 1. Build and install the binary

On the server (or cross-compile on the Mac and scp):

```sh
cargo build --release
sudo install -m 755 target/release/harness-server /usr/local/bin/harness-server
```

## 2. Create a service user

```sh
sudo useradd --system --home /var/lib/voice-harness --shell /usr/sbin/nologin voice-harness
```

## 3. Config and secrets

```sh
sudo mkdir -p /etc/voice-harness
sudo install -m 600 config/voice-harness.toml /etc/voice-harness/voice-harness.toml
```

Edit `/etc/voice-harness/voice-harness.toml`:

- `[server] bind` — `127.0.0.1:8090` to keep it private (reverse proxy in
  front), or `0.0.0.0:8090` / LAN IP for ESP32 clients on the LAN.
- `[stt]` / `[tts]` — voice-server API keys
- `[llm]` — chat endpoint key; remember `base_url + chat_path` concatenate
  (for Open WebUI: `base_url` ends in `/api`, `chat_path` starts with
  `/v1/` — don't double-prefix).

Optional: drop `VH_*` overrides (e.g. `VH_STT__API_KEY=...`) into
`/etc/voice-harness/env` (0600). Env vars win over the file. Keep secrets out
of the unit file itself.

## 4. Install the unit

```sh
sudo cp deploy/voice-harness.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now voice-harness
systemctl status voice-harness
journalctl -u voice-harness -f
```

## 5. Verify

```sh
curl -s http://127.0.0.1:8090/healthz          # → ok
curl -s -X POST http://127.0.0.1:8090/v1/turn \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $KEY" \
  -d '{"text":"what time is it"}'
```

Failure modes:

- `status=203/EXEC` — binary path wrong, or it can't read the config file
  (check ownership: `voice-harness:voice-harness`, 0600).
- exits with "failed to bind" — port taken; change `[server] bind` and restart.
- `config missing required keys` — fill the API keys.

## Upgrade path

Rebuild, `sudo install -m 755 target/release/harness-server
/usr/local/bin/harness-server`, `sudo systemctl restart voice-harness`. Config
changes need only a restart (no daemon-reload; that's for unit changes).

Hardening note: `MemoryDenyWriteExecute=true` is set because the release build
is a plain static-ish binary. If you ever run under a JIT-heavy stack, remove
that line first.
