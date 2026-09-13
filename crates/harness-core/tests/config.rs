//! Config loading tests: defaults, file layering, env overrides, validation.

use harness_core::config::Config;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Env vars are process-global; serialize the tests that touch them.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

fn temp_config_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vh-config-test-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn defaults_parse_with_no_file() {
    let _g = env_lock();
    let cfg = Config::load(None).expect("defaults load");
    assert_eq!(cfg.server.bind, "127.0.0.1:8090");
    assert!(cfg.server.api_keys.is_empty());
    assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8000");
    assert_eq!(cfg.stt.chat_path, "/v1/audio/transcriptions");
    assert_eq!(cfg.stt.model, "asr-model");
    assert_eq!(cfg.stt.api_key, ""); // empty until validate() complains
    assert_eq!(cfg.tts.model, "tts-model");
    assert_eq!(cfg.tts.voice, "default");
    assert_eq!(cfg.tts.response_format, "wav");
    assert_eq!(cfg.llm.base_url, "http://127.0.0.1:3000/api");
    assert_eq!(cfg.llm.chat_path, "/v1/chat/completions");
    assert_eq!(cfg.llm.model, "default");
    assert_eq!(cfg.session.silence_ms, 700);
    assert_eq!(cfg.session.min_utterance_ms, 300);
    assert_eq!(cfg.session.max_utterance_ms, 30_000);
    assert_eq!(cfg.session.pre_speech_ms, 500);
    assert_eq!(cfg.session.chunk_max_chars, 200);
    assert_eq!(cfg.session.max_history_turns, 8);
    assert_eq!(cfg.session.idle_timeout_secs, 600);
    assert!(cfg.prompts.system.contains("voice assistant"));
    assert_eq!(
        cfg.plugins.enabled,
        vec!["time".to_string(), "http_fetch".to_string()]
    );
}

#[test]
fn file_overrides_defaults() {
    let _g = env_lock();
    let dir = temp_config_dir();
    let file = dir.join("voice-harness.toml");
    fs::write(
        &file,
        "[server]\nbind = \"0.0.0.0:9001\"\n\n[session]\nsilence_ms = 900\n\n[stt]\napi_key = \"file-key\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(&file)).expect("file load");
    assert_eq!(cfg.server.bind, "0.0.0.0:9001");
    assert_eq!(cfg.session.silence_ms, 900);
    assert_eq!(cfg.stt.api_key, "file-key");
    // untouched values keep defaults
    assert_eq!(cfg.session.min_utterance_ms, 300);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn env_override_wins_over_file() {
    let _g = env_lock();
    let dir = temp_config_dir();
    let file = dir.join("voice-harness.toml");
    fs::write(&file, "[session]\nsilence_ms = 700\n").unwrap();
    std::env::set_var("VH_SESSION__SILENCE_MS", "250");
    let cfg = Config::load(Some(&file)).expect("env override load");
    std::env::remove_var("VH_SESSION__SILENCE_MS");
    assert_eq!(cfg.session.silence_ms, 250);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn env_overrides_defaults_without_file() {
    let _g = env_lock();
    std::env::set_var("VH_STT__API_KEY", "env-key");
    std::env::set_var("VH_LLM__BASE_URL", "http://example.test/api");
    let cfg = Config::load(None).expect("env load");
    std::env::remove_var("VH_STT__API_KEY");
    std::env::remove_var("VH_LLM__BASE_URL");
    assert_eq!(cfg.stt.api_key, "env-key");
    assert_eq!(cfg.llm.base_url, "http://example.test/api");
}

#[test]
fn validate_lists_missing_api_keys() {
    let _g = env_lock();
    let cfg = Config::load(None).expect("defaults load");
    let errs = cfg.validate().expect_err("missing keys must error");
    assert!(
        errs.iter().any(|e| e.contains("stt.api_key")),
        "errs: {errs:?}"
    );
    assert!(
        errs.iter().any(|e| e.contains("tts.api_key")),
        "errs: {errs:?}"
    );
    assert!(
        errs.iter().any(|e| e.contains("llm.api_key")),
        "errs: {errs:?}"
    );

    let mut ok = cfg.clone();
    ok.stt.api_key = "k".into();
    ok.tts.api_key = "k".into();
    ok.llm.api_key = "k".into();
    assert!(ok.validate().is_ok(), "complete config must validate");
}
