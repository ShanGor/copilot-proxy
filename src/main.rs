use std::{env, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use copilot_proxy::{
    auth::{AuthConfig, CopilotAuthenticator},
    cli::{parse_startup_args, usage},
    config::AppConfig,
    proxy::{ApiKeyProvider, ProxyError, build_proxy_router_with_client},
};
use tokio::net::TcpListener;
use tracing::{error, info};

struct RuntimeApiKeyProvider {
    auth: CopilotAuthenticator,
}

#[async_trait]
impl ApiKeyProvider for RuntimeApiKeyProvider {
    async fn get_api_key(&self) -> Result<String, ProxyError> {
        self.auth
            .get_api_key()
            .await
            .map_err(|e| ProxyError::Other(format!("failed to get Copilot API key: {e}")))
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "copilot_proxy=info".into()),
        )
        .init();

    if let Err(e) = run().await {
        error!("startup failed: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli_args = parse_startup_args(env::args().skip(1)).map_err(|e| {
        if e.starts_with("Usage:") {
            e
        } else {
            format!("{e}\n\n{}", usage())
        }
    })?;

    let config_path = resolve_config_path();
    let raw = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("failed to read config {}: {e}", config_path.display()))?;
    let config = AppConfig::from_yaml_str(&raw).map_err(|e| format!("invalid config: {e}"))?;

    let settings = config.proxy_settings.clone().unwrap_or_default();
    let listen = settings
        .listen
        .unwrap_or_else(|| "0.0.0.0:4141".to_string());
    let upstream_base = settings
        .upstream_base
        .unwrap_or_else(|| "https://api.githubcopilot.com".to_string());
    let transforms = settings.transforms;

    let token_dir = settings
        .auth
        .as_ref()
        .and_then(|a| a.token_dir.clone())
        .map(PathBuf::from)
        .unwrap_or_else(default_token_dir);
    let github_api_key_url = settings
        .auth
        .as_ref()
        .and_then(|a| a.github_api_key_url.clone())
        .unwrap_or_else(|| "https://api.github.com/copilot_internal/v2/token".to_string());

    let http_client = build_http_client(cli_args.proxy.as_deref())?;
    let auth = CopilotAuthenticator::new_with_client(
        AuthConfig {
            token_dir: token_dir.clone(),
            github_api_key_url: github_api_key_url.clone(),
            github_device_code_url: "https://github.com/login/device/code".to_string(),
            github_access_token_url: "https://github.com/login/oauth/access_token".to_string(),
            github_client_id: "Iv1.b507a08c87ecfe98".to_string(),
        },
        http_client.clone(),
    );
    info!(
        token_dir = %token_dir.display(),
        github_api_key_url = %github_api_key_url,
        "copilot auth configured"
    );
    auth
        .get_api_key()
        .await
        .map_err(|e| format!("copilot auth preflight failed: {e}"))?;
    info!("copilot auth preflight succeeded");

    let provider: Arc<dyn ApiKeyProvider> = Arc::new(RuntimeApiKeyProvider { auth });
    let app = build_proxy_router_with_client(
        config,
        upstream_base,
        transforms,
        provider,
        http_client,
    );

    let listener = TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("failed to bind {listen}: {e}"))?;

    info!("copilot proxy listening on {listen}");
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("server error: {e}"))
}

fn build_http_client(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().danger_accept_invalid_certs(true);
    info!("TLS certificate verification is disabled for outbound requests");
    if let Some(proxy_url) = proxy {
        // When --proxy is explicitly provided, do not let env proxy/NO_PROXY rules override it.
        builder = builder.no_proxy();
        let proxy = reqwest::Proxy::all(proxy_url)
            .map_err(|e| format!("invalid --proxy value `{proxy_url}`: {e}"))?;
        builder = builder.proxy(proxy);
        info!(proxy = %proxy_url, "outbound proxy enabled via --proxy");
    } else {
        info!("outbound proxy not explicitly set; reqwest env proxy behavior may apply");
    }

    builder
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))
}

fn resolve_config_path() -> PathBuf {
    if let Ok(p) = env::var("COPILOT_PROXY_CONFIG") {
        return PathBuf::from(p);
    }

    let local = PathBuf::from("config.yaml");
    if local.exists() {
        return local;
    }

    let parent = PathBuf::from("../config.yaml");
    if parent.exists() {
        return parent;
    }

    local
}

fn default_token_dir() -> PathBuf {
    if let Ok(v) = env::var("GITHUB_COPILOT_TOKEN_DIR") {
        return PathBuf::from(v);
    }

    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".config/litellm/github_copilot");
    }

    PathBuf::from(".copilot-tokens")
}
