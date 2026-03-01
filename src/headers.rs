use std::collections::BTreeMap;
use uuid::Uuid;

pub fn build_copilot_headers(api_key: &str) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_string(), format!("Bearer {api_key}"));
    headers.insert("content-type".to_string(), "application/json".to_string());
    headers.insert(
        "copilot-integration-id".to_string(),
        "vscode-chat".to_string(),
    );
    headers.insert("editor-version".to_string(), "vscode/1.95.0".to_string());
    headers.insert(
        "editor-plugin-version".to_string(),
        "copilot-chat/0.26.7".to_string(),
    );
    headers.insert(
        "user-agent".to_string(),
        "GitHubCopilotChat/0.26.7".to_string(),
    );
    headers.insert("openai-intent".to_string(), "conversation-panel".to_string());
    headers.insert("x-github-api-version".to_string(), "2025-04-01".to_string());
    headers.insert("x-request-id".to_string(), Uuid::new_v4().to_string());
    headers.insert(
        "x-vscode-user-agent-library-version".to_string(),
        "electron-fetch".to_string(),
    );
    headers
}

#[cfg(test)]
mod tests {
    use super::build_copilot_headers;

    #[test]
    fn includes_expected_vscode_headers() {
        let headers = build_copilot_headers("token-123");
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer token-123")
        );
        assert_eq!(
            headers.get("copilot-integration-id").map(String::as_str),
            Some("vscode-chat")
        );
        assert!(headers.get("editor-version").is_some());
        assert!(headers.get("user-agent").is_some());
        assert!(headers.get("x-request-id").is_some());
    }
}
