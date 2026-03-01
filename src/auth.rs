use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub token_dir: PathBuf,
    pub github_api_key_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyFile {
    pub token: String,
    pub expires_at: f64,
}

#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth error: {0}")]
    Other(String),
}

pub struct CopilotAuthenticator {
    cfg: AuthConfig,
    http: reqwest::Client,
}

impl CopilotAuthenticator {
    pub fn new(cfg: AuthConfig) -> Self {
        Self::new_with_client(cfg, reqwest::Client::new())
    }

    pub fn new_with_client(cfg: AuthConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
        }
    }

    pub async fn get_api_key(&self) -> Result<String, AuthError> {
        std::fs::create_dir_all(&self.cfg.token_dir)?;

        if let Some(existing) = self.read_api_key_file().ok()
            && existing.expires_at > now_ts()
        {
            return Ok(existing.token);
        }

        let access_token = self.read_access_token()?;
        let refreshed = self.refresh_api_key(&access_token).await?;
        self.write_api_key_file(&refreshed)?;
        Ok(refreshed.token)
    }

    fn access_token_path(&self) -> PathBuf {
        self.cfg.token_dir.join("access-token")
    }

    fn api_key_path(&self) -> PathBuf {
        self.cfg.token_dir.join("api-key.json")
    }

    fn read_access_token(&self) -> Result<String, AuthError> {
        let token = std::fs::read_to_string(self.access_token_path())?;
        let trimmed = token.trim().to_string();
        if trimmed.is_empty() {
            return Err(AuthError::Other("access token file is empty".to_string()));
        }
        Ok(trimmed)
    }

    fn read_api_key_file(&self) -> Result<ApiKeyFile, AuthError> {
        let bytes = std::fs::read(self.api_key_path())?;
        let parsed: ApiKeyFile = serde_json::from_slice(&bytes)?;
        Ok(parsed)
    }

    fn write_api_key_file(&self, api_key: &ApiKeyFile) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(api_key)?;
        std::fs::write(self.api_key_path(), bytes)?;
        Ok(())
    }

    async fn refresh_api_key(&self, access_token: &str) -> Result<ApiKeyFile, AuthError> {
        let resp: serde_json::Value = self
            .http
            .get(&self.cfg.github_api_key_url)
            .header("authorization", format!("token {access_token}"))
            .header("accept", "application/json")
            .header("editor-version", "vscode/1.85.1")
            .header("editor-plugin-version", "copilot/1.155.0")
            .header("user-agent", "GithubCopilot/1.155.0")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let token = resp
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AuthError::Other("api key response missing token".to_string()))?
            .to_string();

        let expires_at = resp
            .get("expires_at")
            .and_then(|v| v.as_f64())
            .unwrap_or_else(|| now_ts() + 3600.0);

        Ok(ApiKeyFile { token, expires_at })
    }
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    use super::{ApiKeyFile, AuthConfig, CopilotAuthenticator};

    #[derive(Clone)]
    struct MockState {
        hits: Arc<AtomicUsize>,
    }

    async fn token_handler(State(state): State<MockState>) -> impl IntoResponse {
        state.hits.fetch_add(1, Ordering::SeqCst);
        Json(json!({
            "token": "new-api-key",
            "expires_at": now_ts() + 3600.0,
        }))
    }

    fn now_ts() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_secs_f64()
    }

    async fn start_mock_server() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/copilot-token", get(token_handler))
            .with_state(MockState { hits: hits.clone() });
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });
        (format!("http://{addr}/copilot-token"), hits)
    }

    fn write_api_key_file(tmp: &TempDir, token: &str, expires_at: f64) {
        let file = ApiKeyFile {
            token: token.to_string(),
            expires_at,
        };
        let path = tmp.path().join("api-key.json");
        std::fs::write(
            path,
            serde_json::to_vec(&file).expect("serialize api key file"),
        )
        .expect("write api key file");
    }

    fn write_access_token_file(tmp: &TempDir, token: &str) {
        std::fs::write(tmp.path().join("access-token"), token).expect("write access token");
    }

    #[tokio::test]
    async fn returns_cached_api_key_when_not_expired() {
        let tmp = TempDir::new().expect("temp dir");
        write_api_key_file(&tmp, "cached-key", now_ts() + 3600.0);
        write_access_token_file(&tmp, "gh-access");
        let cfg = AuthConfig {
            token_dir: tmp.path().to_path_buf(),
            github_api_key_url: "http://127.0.0.1:9/unreachable".to_string(),
        };

        let auth = CopilotAuthenticator::new(cfg);
        let token = auth.get_api_key().await.expect("cached key should return");
        assert_eq!(token, "cached-key");
    }

    #[tokio::test]
    async fn refreshes_api_key_and_persists_it() {
        let tmp = TempDir::new().expect("temp dir");
        write_access_token_file(&tmp, "gh-access");
        let (url, hits) = start_mock_server().await;

        let cfg = AuthConfig {
            token_dir: tmp.path().to_path_buf(),
            github_api_key_url: url,
        };
        let auth = CopilotAuthenticator::new(cfg);
        let token = auth
            .get_api_key()
            .await
            .expect("refresh should fetch new token");

        assert_eq!(token, "new-api-key");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let stored = std::fs::read(tmp.path().join("api-key.json")).expect("read stored file");
        let parsed: ApiKeyFile = serde_json::from_slice(&stored).expect("parse stored file");
        assert_eq!(parsed.token, "new-api-key");
    }
}
