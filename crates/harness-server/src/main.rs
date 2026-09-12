//! Server binary: load config, build real provider clients, serve the router.

use std::sync::Arc;

use harness_core::config::Config;
use harness_plugins::registry_from_config;
use harness_providers::llm::OpenAiLlmClient;
use harness_providers::stt::OpenAiSttClient;
use harness_providers::tts::OpenAiTtsClient;
use harness_server::http::{build_router, RouterDeps};
use harness_server::state::SessionStore;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::path::PathBuf::from("config/voice-harness.toml")
                .exists()
                .then_some(std::path::PathBuf::from("config/voice-harness.toml"))
        });

    let config = Config::load(config_path.as_deref()).unwrap_or_else(|e| {
        eprintln!("failed to load config: {e}");
        std::process::exit(1);
    });
    if let Err(missing) = config.validate() {
        eprintln!(
            "config missing required keys: {} (set them in the file or via VH_<SECTION>__<KEY> env vars)",
            missing.join(", ")
        );
        std::process::exit(1);
    }

    let deps = RouterDeps {
        sessions: Arc::new(SessionStore::new()),
        llm: Arc::new(OpenAiLlmClient::new(
            &config.llm.base_url,
            &config.llm.chat_path,
            &config.llm.api_key,
        )),
        tts: Arc::new(OpenAiTtsClient::from_config(&config.tts)),
        stt: Arc::new(OpenAiSttClient::from_config(&config.stt)),
        plugins: Arc::new(registry_from_config(&config.plugins)),
        config: config.clone(),
    };

    let app = build_router(deps);
    let listener = tokio::net::TcpListener::bind(&config.server.bind)
        .await
        .unwrap_or_else(|e| {
            eprintln!("failed to bind {}: {e}", config.server.bind);
            std::process::exit(1);
        });
    tracing::info!("voice-harness listening on http://{}", config.server.bind);
    axum::serve(listener, app)
        .await
        .expect("server runs until stopped");
}
