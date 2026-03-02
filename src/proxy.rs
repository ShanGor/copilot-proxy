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
use futures_util::TryStreamExt;
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
    let is_stream_request = parsed_request_json
        .as_ref()
        .and_then(|body| body.get("stream"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_event_stream = is_event_stream_response(&resp_headers);

    let mut response = if is_stream_request || is_event_stream {
        debug!(
            route = %incoming_path,
            upstream_route = %upstream_path,
            status = %status,
            "streaming upstream response payload"
        );
        let stream = upstream_resp.bytes_stream().map_err(std::io::Error::other);
        Response::builder()
            .status(status)
            .body(Body::from_stream(stream))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
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
        Response::builder()
            .status(status)
            .body(Body::from(resp_bytes))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

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

fn is_event_stream_response(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|content_type| content_type.to_ascii_lowercase().starts_with("text/event-stream"))
        .unwrap_or(false)
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
        time::{Duration, Instant},
    };

    use axum::{
        Json, Router,
        body::{Body, Bytes},
        extract::State,
        http::{HeaderMap, Request, StatusCode},
        response::{IntoResponse, Response},
        routing::post,
    };
    use futures_util::{StreamExt, stream};
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
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            let chunks = vec![
                "data: {\"delta\":\"a\"}\n\n",
                "data: {\"delta\":\"b\"}\n\n",
                "data: [DONE]\n\n",
            ];
            let sse = stream::unfold((0usize, chunks), |(idx, chunks)| async move {
                if idx >= chunks.len() {
                    None
                } else {
                    if idx > 0 {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    Some((
                        Ok::<Bytes, std::io::Error>(Bytes::from(chunks[idx].to_string())),
                        (idx + 1, chunks),
                    ))
                }
            });
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(sse))
                .expect("build sse response")
                .into_response();
        }
        (StatusCode::OK, Json(json!({"ok": true, "echo": body}))).into_response()
    }

    async fn start_upstream() -> (String, Arc<Mutex<Option<Value>>>, Arc<Mutex<Option<HeaderMap>>>) {
        let last_body = Arc::new(Mutex::new(None));
        let last_headers = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/chat/completions", post(upstream_handler))
            .route("/v1/responses", post(upstream_handler))
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

    fn assert_common_copilot_headers(headers: &HeaderMap) {
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer proxy-api-key")
        );
        assert_eq!(
            headers
                .get("copilot-integration-id")
                .and_then(|v| v.to_str().ok()),
            Some("vscode-chat")
        );
        assert_eq!(
            headers
                .get("editor-version")
                .and_then(|v| v.to_str().ok()),
            Some("vscode/1.95.0")
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

    #[tokio::test]
    async fn applies_copilot_headers_for_chat_and_responses_endpoints() {
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

        let chat_req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": "gpt-4o",
                    "messages": [{"role": "user", "content": "hello"}]
                })
                .to_string(),
            ))
            .expect("build chat request");
        let chat_res = app.clone().oneshot(chat_req).await.expect("chat response");
        assert_eq!(chat_res.status(), StatusCode::OK);
        let chat_headers = last_headers
            .lock()
            .expect("lock upstream headers")
            .clone()
            .expect("upstream should receive chat headers");
        assert_common_copilot_headers(&chat_headers);

        let responses_req = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": "gpt-4o",
                    "input": "hello"
                })
                .to_string(),
            ))
            .expect("build responses request");
        let responses_res = app
            .oneshot(responses_req)
            .await
            .expect("responses response");
        assert_eq!(responses_res.status(), StatusCode::OK);
        let responses_headers = last_headers
            .lock()
            .expect("lock upstream headers")
            .clone()
            .expect("upstream should receive responses headers");
        assert_common_copilot_headers(&responses_headers);
    }

    #[tokio::test]
    async fn streams_chat_completions_without_buffering() {
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

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("proxy server run");
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/v1/chat/completions"))
            .json(&json!({
                "model": "gpt-4o",
                "stream": true,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("send request");

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream"))
        );

        let mut body = resp.bytes_stream();
        let start = Instant::now();
        let first_chunk = tokio::time::timeout(Duration::from_millis(150), body.next())
            .await
            .expect("first stream chunk timed out")
            .expect("first stream chunk missing")
            .expect("first stream chunk error");
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "first chunk arrived too late, likely buffered"
        );
        assert!(String::from_utf8_lossy(&first_chunk).contains("data:"));
    }

    #[tokio::test]
    async fn streams_responses_without_buffering() {
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

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("proxy server run");
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/v1/responses"))
            .json(&json!({
                "model": "gpt-4o",
                "stream": true,
                "input": "hello"
            }))
            .send()
            .await
            .expect("send request");

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream"))
        );

        let mut body = resp.bytes_stream();
        let start = Instant::now();
        let first_chunk = tokio::time::timeout(Duration::from_millis(150), body.next())
            .await
            .expect("first stream chunk timed out")
            .expect("first stream chunk missing")
            .expect("first stream chunk error");
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "first chunk arrived too late, likely buffered"
        );
        assert!(String::from_utf8_lossy(&first_chunk).contains("data:"));
    }
}
