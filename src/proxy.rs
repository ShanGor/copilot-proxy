use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::Response,
    routing::any,
};
use serde_json::Value;
use tracing::debug;

use crate::{
    config::AppConfig,
    headers::build_copilot_headers,
    transform::{RequestContext, TransformRule, apply_transforms},
};

#[derive(thiserror::Error, Debug)]
pub enum ProxyError {
    #[error("proxy error: {0}")]
    Other(String),
}

#[async_trait]
pub trait ApiKeyProvider: Send + Sync {
    async fn get_api_key(&self) -> Result<String, ProxyError>;
}

pub struct StaticApiKeyProvider {
    token: String,
}

impl StaticApiKeyProvider {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

#[async_trait]
impl ApiKeyProvider for StaticApiKeyProvider {
    async fn get_api_key(&self) -> Result<String, ProxyError> {
        Ok(self.token.clone())
    }
}

#[derive(Clone)]
struct ProxyState {
    config: AppConfig,
    upstream_base: String,
    transforms: Vec<TransformRule>,
    api_key_provider: Arc<dyn ApiKeyProvider>,
    http: reqwest::Client,
}

pub fn build_proxy_router(
    config: AppConfig,
    upstream_base: String,
    transforms: Vec<TransformRule>,
    api_key_provider: Arc<dyn ApiKeyProvider>,
) -> Router {
    build_proxy_router_with_client(
        config,
        upstream_base,
        transforms,
        api_key_provider,
        reqwest::Client::new(),
    )
}

pub fn build_proxy_router_with_client(
    config: AppConfig,
    upstream_base: String,
    transforms: Vec<TransformRule>,
    api_key_provider: Arc<dyn ApiKeyProvider>,
    http: reqwest::Client,
) -> Router {
    let state = ProxyState {
        config,
        upstream_base,
        transforms,
        api_key_provider,
        http,
    };

    Router::new()
        .route("/", any(proxy_handler))
        .route("/{*path}", any(proxy_handler))
        .with_state(Arc::new(state))
}

async fn proxy_handler(
    State(state): State<Arc<ProxyState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, StatusCode> {
    let mut outbound_body = body.to_vec();
    let incoming_path = uri.path().to_string();
    let upstream_path = normalize_upstream_path(&incoming_path);
    let mut parsed_request_json: Option<Value> = None;
    debug!(
        route = %incoming_path,
        upstream_route = %upstream_path,
        method = %method,
        incoming_payload = %String::from_utf8_lossy(&outbound_body),
        "incoming request payload"
    );

    if let Ok(mut json_body) = serde_json::from_slice::<Value>(&outbound_body) {
        let incoming_model = json_body
            .get("model")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);

        if let Some(alias) = incoming_model.as_deref()
            && let Some(upstream_model) = state.config.resolve_upstream_model(alias)
            && let Some(obj) = json_body.as_object_mut()
        {
            obj.insert("model".to_string(), Value::String(upstream_model.to_string()));
        }

        apply_transforms(
            &mut json_body,
            &state.transforms,
            RequestContext {
                route: &incoming_path,
                model: incoming_model.as_deref(),
            },
        );

        normalize_copilot_model_field(&mut json_body);
        parsed_request_json = Some(json_body.clone());

        outbound_body = serde_json::to_vec(&json_body).map_err(|_| StatusCode::BAD_REQUEST)?;
    }

    let upstream_url = if let Some(q) = uri.query() {
        format!(
            "{}{}?{q}",
            state.upstream_base.trim_end_matches('/'),
            upstream_path
        )
    } else {
        format!("{}{}", state.upstream_base.trim_end_matches('/'), upstream_path)
    };

    let api_key = state
        .api_key_provider
        .get_api_key()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let mut req = state.http.request(method, upstream_url);

    for (name, value) in &headers {
        if name.as_str().eq_ignore_ascii_case("host")
            || name.as_str().eq_ignore_ascii_case("content-length")
            || name.as_str().eq_ignore_ascii_case("authorization")
        {
            continue;
        }
        req = req.header(name, value);
    }

    for (name, value) in build_copilot_headers(&api_key) {
        req = req.header(name, value);
    }
    if let Some(json_body) = parsed_request_json.as_ref() {
        req = add_dynamic_copilot_headers(req, json_body);
    }
    req = req.body(outbound_body);

    let upstream_resp = req.send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let resp_bytes = upstream_resp
        .bytes()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    debug!(
        route = %incoming_path,
        upstream_route = %upstream_path,
        status = %status,
        response_payload = %String::from_utf8_lossy(&resp_bytes),
        "upstream response payload"
    );

    let mut response = Response::builder()
        .status(status)
        .body(Body::from(resp_bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let out_headers = response.headers_mut();
    for (name, value) in resp_headers {
        if let Some(name) = name
            && !name.as_str().eq_ignore_ascii_case("transfer-encoding")
            && !name.as_str().eq_ignore_ascii_case("content-length")
        {
            out_headers.insert(name, value);
        }
    }

    let _ = out_headers.insert(
        HeaderName::from_static("x-copilot-proxy"),
        HeaderValue::from_static("rust"),
    );
    Ok(response)
}

fn normalize_copilot_model_field(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(model) = obj.get("model").and_then(|v| v.as_str()) else {
        return;
    };
    if let Some(stripped) = model.strip_prefix("github_copilot/") {
        obj.insert("model".to_string(), Value::String(stripped.to_string()));
    }
}

fn normalize_upstream_path(path: &str) -> &str {
    if path == "/v1/chat/completions" {
        "/chat/completions"
    } else {
        path
    }
}

fn add_dynamic_copilot_headers(mut req: reqwest::RequestBuilder, body: &Value) -> reqwest::RequestBuilder {
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        let mut initiator = "user";
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or_default();
            if role == "assistant" || role == "tool" {
                initiator = "agent";
                break;
            }
        }
        req = req.header("X-Initiator", initiator);

        let has_vision = messages.iter().any(message_has_vision);
        if has_vision {
            req = req.header("Copilot-Vision-Request", "true");
        }
    }
    req
}

fn message_has_vision(msg: &Value) -> bool {
    let Some(content) = msg.get("content") else {
        return false;
    };
    let Some(items) = content.as_array() else {
        return false;
    };

    items.iter().any(|item| {
        item.get("image_url").is_some()
            || item.get("type").and_then(|v| v.as_str()) == Some("image_url")
    })
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use axum::{
        Json, Router,
        body::Body,
        extract::State,
        http::{HeaderMap, Request, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tower::util::ServiceExt;

    use crate::{
        config::AppConfig,
        proxy::{StaticApiKeyProvider, build_proxy_router},
        transform::{TransformOp, TransformRule},
    };

    #[derive(Clone)]
    struct UpstreamState {
        last_body: Arc<Mutex<Option<Value>>>,
        last_headers: Arc<Mutex<Option<HeaderMap>>>,
    }

    async fn upstream_handler(
        State(state): State<UpstreamState>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        *state.last_body.lock().expect("lock") = Some(body.clone());
        *state.last_headers.lock().expect("lock") = Some(headers);
        (StatusCode::OK, Json(json!({"ok": true, "echo": body})))
    }

    async fn start_upstream() -> (String, Arc<Mutex<Option<Value>>>, Arc<Mutex<Option<HeaderMap>>>) {
        let last_body = Arc::new(Mutex::new(None));
        let last_headers = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/chat/completions", post(upstream_handler))
            .with_state(UpstreamState {
                last_body: last_body.clone(),
                last_headers: last_headers.clone(),
            });

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server run");
        });
        (format!("http://{addr}"), last_body, last_headers)
    }

    #[tokio::test]
    async fn rewrites_model_alias_and_applies_request_transforms() {
        let (upstream, last_body, _last_headers) = start_upstream().await;
        let yaml = r#"
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: github_copilot/gpt-4o-2024-11-20
"#;
        let config = AppConfig::from_yaml_str(yaml).expect("parse config");
        let transforms = vec![TransformRule {
            when: None,
            ops: vec![
                TransformOp::Remove {
                    path: "$.temperature".to_string(),
                },
                TransformOp::Add {
                    path: "$.metadata.source".to_string(),
                    value: json!("proxy"),
                },
            ],
        }];

        let app = build_proxy_router(
            config,
            upstream,
            transforms,
            Arc::new(StaticApiKeyProvider::new("proxy-api-key")),
        );

        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": "gpt-4o",
                    "temperature": 0.7,
                    "messages": [{"role": "user", "content": "hello"}]
                })
                .to_string(),
            ))
            .expect("build request");

        let res = app.oneshot(req).await.expect("proxy response");
        assert_eq!(res.status(), StatusCode::OK);

        let forwarded = last_body
            .lock()
            .expect("lock upstream body")
            .clone()
            .expect("upstream should receive body");

        assert_eq!(
            forwarded["model"],
            json!("gpt-4o-2024-11-20")
        );
        assert!(forwarded.get("temperature").is_none());
        assert_eq!(forwarded["metadata"]["source"], json!("proxy"));
    }

    #[tokio::test]
    async fn adds_x_initiator_and_vision_headers() {
        let (upstream, _last_body, last_headers) = start_upstream().await;
        let yaml = r#"
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: github_copilot/gpt-4o-2024-11-20
"#;
        let config = AppConfig::from_yaml_str(yaml).expect("parse config");
        let app = build_proxy_router(
            config,
            upstream,
            vec![],
            Arc::new(StaticApiKeyProvider::new("proxy-api-key")),
        );

        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": "gpt-4o",
                    "messages": [{
                        "role": "assistant",
                        "content": [{"type": "image_url", "image_url": {"url": "https://x"}}]
                    }]
                })
                .to_string(),
            ))
            .expect("build request");

        let res = app.oneshot(req).await.expect("proxy response");
        assert_eq!(res.status(), StatusCode::OK);

        let headers = last_headers
            .lock()
            .expect("lock upstream headers")
            .clone()
            .expect("upstream should receive headers");
        assert_eq!(
            headers.get("x-initiator").and_then(|v| v.to_str().ok()),
            Some("agent")
        );
        assert_eq!(
            headers
                .get("copilot-vision-request")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
    }

    #[tokio::test]
    async fn forwards_chat_completions_without_v1_prefix() {
        let (upstream, _last_body, _last_headers) = start_upstream().await;
        let yaml = r#"
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: github_copilot/gpt-4o-2024-11-20
"#;
        let config = AppConfig::from_yaml_str(yaml).expect("parse config");
        let app = build_proxy_router(
            config,
            upstream,
            vec![],
            Arc::new(StaticApiKeyProvider::new("proxy-api-key")),
        );

        let req = Request::builder()
            .method("POST")
            .uri("/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": "gpt-4o",
                    "messages": [{"role": "user", "content": "hello"}]
                })
                .to_string(),
            ))
            .expect("build request");

        let res = app.oneshot(req).await.expect("proxy response");
        assert_eq!(res.status(), StatusCode::OK);
    }
}
