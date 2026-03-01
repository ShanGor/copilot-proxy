use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub token_dir: PathBuf,
    pub github_api_key_url: String,
    pub github_device_code_url: String,
    pub github_access_token_url: String,
    pub github_client_id: String,
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
        debug!(token_dir = %self.cfg.token_dir.display(), "checking copilot token directory");

        match self.read_api_key_file() {
            Ok(existing) if existing.expires_at > now_ts() => {
                debug!(expires_at = existing.expires_at, "using cached copilot api key from api-key.json");
                return Ok(existing.token);
            }
            Ok(_) => {
                info!("cached copilot api key is expired, refreshing");
            }
            Err(e) => {
                warn!("no readable cached copilot api key found: {e}");
            }
        }
        debug!("no valid cached copilot api key found, refreshing");

        let access_token = self.get_access_token().await?;
        let refreshed = self.refresh_api_key(&access_token).await?;
        self.write_api_key_file(&refreshed)?;
        debug!(expires_at = refreshed.expires_at, "stored refreshed copilot api key to api-key.json");
        Ok(refreshed.token)
    }

    async fn get_access_token(&self) -> Result<String, AuthError> {
        match self.read_access_token() {
            Ok(token) => Ok(token),
            Err(e) => {
                warn!("no readable github access token found, starting device code auth flow: {e}");
                let new_token = self.login_device_flow().await?;
                self.write_access_token(&new_token)?;
                Ok(new_token)
            }
        }
    }

    fn access_token_path(&self) -> PathBuf {
        self.cfg.token_dir.join("access-token")
    }

    fn api_key_path(&self) -> PathBuf {
        self.cfg.token_dir.join("api-key.json")
    }

    fn read_access_token(&self) -> Result<String, AuthError> {
        let path = self.access_token_path();
        debug!(path = %path.display(), "reading github access token file");
        let token = std::fs::read_to_string(&path).map_err(|e| {
            warn!(path = %path.display(), "failed to read github access token file: {e}");
            AuthError::Io(e)
        })?;
        let trimmed = token.trim().to_string();
        if trimmed.is_empty() {
            warn!("github access token file exists but is empty");
            return Err(AuthError::Other("access token file is empty".to_string()));
        }
        Ok(trimmed)
    }

    fn write_access_token(&self, token: &str) -> Result<(), AuthError> {
        let path = self.access_token_path();
        debug!(path = %path.display(), "writing github access token file");
        std::fs::write(path, token)?;
        Ok(())
    }

    fn read_api_key_file(&self) -> Result<ApiKeyFile, AuthError> {
        let path = self.api_key_path();
        debug!(path = %path.display(), "reading cached copilot api key file");
        let bytes = std::fs::read(path)?;
        let parsed: ApiKeyFile = serde_json::from_slice(&bytes)?;
        Ok(parsed)
    }

    fn write_api_key_file(&self, api_key: &ApiKeyFile) -> Result<(), AuthError> {
        let path = self.api_key_path();
        debug!(path = %path.display(), "writing cached copilot api key file");
        let bytes = serde_json::to_vec(api_key)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    async fn refresh_api_key(&self, access_token: &str) -> Result<ApiKeyFile, AuthError> {
        debug!(url = %self.cfg.github_api_key_url, "requesting refreshed copilot api key from github");
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

    async fn login_device_flow(&self) -> Result<String, AuthError> {
        let device_info = self.get_device_code().await?;
        println!(
            "Please visit {} and enter code {} to authenticate GitHub Copilot.",
            device_info.verification_uri, device_info.user_code
        );
        self.poll_for_access_token(&device_info.device_code, device_info.interval)
            .await
    }

    async fn get_device_code(&self) -> Result<DeviceCodeResponse, AuthError> {
        debug!(url = %self.cfg.github_device_code_url, "requesting github device code");
        self.http
            .post(&self.cfg.github_device_code_url)
            .header("accept", "application/json")
            .header("editor-version", "vscode/1.85.1")
            .header("editor-plugin-version", "copilot/1.155.0")
            .header("user-agent", "GithubCopilot/1.155.0")
            .json(&serde_json::json!({
                "client_id": self.cfg.github_client_id,
                "scope": "read:user"
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<DeviceCodeResponse>()
            .await
            .map_err(AuthError::from)
    }

    async fn poll_for_access_token(
        &self,
        device_code: &str,
        interval_seconds: Option<u64>,
    ) -> Result<String, AuthError> {
        let interval = interval_seconds.unwrap_or(5).max(1);
        let max_attempts = 60u32;
        for attempt in 1..=max_attempts {
            debug!(attempt, max_attempts, "polling github oauth access token");
            let resp: AccessTokenPollResponse = self
                .http
                .post(&self.cfg.github_access_token_url)
                .header("accept", "application/json")
                .header("editor-version", "vscode/1.85.1")
                .header("editor-plugin-version", "copilot/1.155.0")
                .header("user-agent", "GithubCopilot/1.155.0")
                .json(&serde_json::json!({
                    "client_id": self.cfg.github_client_id,
                    "device_code": device_code,
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            if let Some(token) = resp.access_token {
                info!("github device auth successful");
                return Ok(token);
            }

            if resp.error.as_deref() == Some("authorization_pending") {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                continue;
            }

            return Err(AuthError::Other(format!(
                "github device auth failed: {}",
                resp.error.unwrap_or_else(|| "unknown error".to_string())
            )));
        }

        Err(AuthError::Other(
            "timed out waiting for github device authorization".to_string(),
        ))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct AccessTokenPollResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
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

    fn test_cfg(tmp: &TempDir, github_api_key_url: String) -> AuthConfig {
        AuthConfig {
            token_dir: tmp.path().to_path_buf(),
            github_api_key_url,
            github_device_code_url: "http://127.0.0.1:9/device-code-unused".to_string(),
            github_access_token_url: "http://127.0.0.1:9/access-token-unused".to_string(),
            github_client_id: "test-client-id".to_string(),
        }
    }

    #[tokio::test]
    async fn returns_cached_api_key_when_not_expired() {
        let tmp = TempDir::new().expect("temp dir");
        write_api_key_file(&tmp, "cached-key", now_ts() + 3600.0);
        write_access_token_file(&tmp, "gh-access");
        let cfg = test_cfg(&tmp, "http://127.0.0.1:9/unreachable".to_string());

        let auth = CopilotAuthenticator::new(cfg);
        let token = auth.get_api_key().await.expect("cached key should return");
        assert_eq!(token, "cached-key");
    }

    #[tokio::test]
    async fn refreshes_api_key_and_persists_it() {
        let tmp = TempDir::new().expect("temp dir");
        write_access_token_file(&tmp, "gh-access");
        let (url, hits) = start_mock_server().await;

        let cfg = test_cfg(&tmp, url);
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

    async fn device_code_handler() -> impl IntoResponse {
        Json(json!({
            "device_code": "dev-code-1",
            "user_code": "USER-CODE",
            "verification_uri": "https://github.com/login/device",
            "interval": 0
        }))
    }

    async fn oauth_token_handler() -> impl IntoResponse {
        Json(json!({
            "access_token": "gh-access-from-device-flow"
        }))
    }

    async fn start_full_auth_server() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/copilot-token", get(token_handler))
            .route("/device-code", axum::routing::post(device_code_handler))
            .route("/oauth-token", axum::routing::post(oauth_token_handler))
            .with_state(MockState { hits: hits.clone() });
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test]
    async fn when_no_access_token_file_runs_device_flow_then_refreshes_api_key() {
        let tmp = TempDir::new().expect("temp dir");
        let (base, hits) = start_full_auth_server().await;
        let cfg = AuthConfig {
            token_dir: tmp.path().to_path_buf(),
            github_api_key_url: format!("{base}/copilot-token"),
            github_device_code_url: format!("{base}/device-code"),
            github_access_token_url: format!("{base}/oauth-token"),
            github_client_id: "test-client-id".to_string(),
        };
        let auth = CopilotAuthenticator::new(cfg);
        let token = auth.get_api_key().await.expect("device flow should succeed");
        assert_eq!(token, "new-api-key");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let access = std::fs::read_to_string(tmp.path().join("access-token"))
            .expect("access token should be written");
        assert_eq!(access, "gh-access-from-device-flow");
    }
}
