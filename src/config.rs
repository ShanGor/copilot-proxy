use serde::Deserialize;

use crate::transform::TransformRule;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub model_list: Vec<ModelEntry>,
    #[serde(default)]
    pub proxy_settings: Option<ProxySettings>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ModelEntry {
    pub model_name: String,
    pub model_info: Option<ModelInfo>,
    pub litellm_params: LiteLLMParams,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ModelInfo {
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LiteLLMParams {
    pub model: String,
    pub drop_params: Option<bool>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ProxySettings {
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub upstream_base: Option<String>,
    #[serde(default)]
    pub auth: Option<AuthSettings>,
    #[serde(default)]
    pub transforms: Vec<TransformRule>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct AuthSettings {
    #[serde(default)]
    pub token_dir: Option<String>,
    #[serde(default)]
    pub github_api_key_url: Option<String>,
}

impl AppConfig {
    pub fn from_yaml_str(yaml: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    pub fn resolve_upstream_model<'a>(&'a self, alias: &str) -> Option<&'a str> {
        self.model_list
            .iter()
            .find(|entry| entry.model_name == alias)
            .map(|entry| entry.litellm_params.model.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::AppConfig;

    #[test]
    fn parses_litellm_style_model_list() {
        let yaml = r#"
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: github_copilot/gpt-4o-2024-11-20
  - model_name: gpt-5-mini
    model_info:
      mode: responses
    litellm_params:
      model: github_copilot/gpt-5-mini
      drop_params: true
"#;
        let cfg = AppConfig::from_yaml_str(yaml).expect("config parse should succeed");
        assert_eq!(cfg.model_list.len(), 2);
        assert_eq!(cfg.model_list[1].model_name, "gpt-5-mini");
        assert_eq!(
            cfg.model_list[1]
                .model_info
                .as_ref()
                .and_then(|mi| mi.mode.as_deref()),
            Some("responses")
        );
        assert_eq!(cfg.model_list[1].litellm_params.drop_params, Some(true));
    }

    #[test]
    fn resolves_upstream_model_from_alias() {
        let yaml = r#"
model_list:
  - model_name: gpt-4.1
    litellm_params:
      model: github_copilot/gpt-4.1-2025-04-14
"#;
        let cfg = AppConfig::from_yaml_str(yaml).expect("config parse should succeed");
        assert_eq!(
            cfg.resolve_upstream_model("gpt-4.1"),
            Some("github_copilot/gpt-4.1-2025-04-14")
        );
        assert_eq!(cfg.resolve_upstream_model("does-not-exist"), None);
    }

    #[test]
    fn supports_optional_proxy_settings() {
        let yaml = r#"
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: github_copilot/gpt-4o-2024-11-20
proxy_settings:
  listen: 0.0.0.0:4141
"#;
        let cfg = AppConfig::from_yaml_str(yaml).expect("config parse should succeed");
        assert_eq!(
            cfg.proxy_settings.and_then(|s| s.listen),
            Some("0.0.0.0:4141".to_string())
        );
    }

    #[test]
    fn parses_proxy_settings_transforms() {
        let yaml = r#"
model_list:
  - model_name: gpt-4o
    litellm_params:
      model: github_copilot/gpt-4o-2024-11-20
proxy_settings:
  upstream_base: https://api.githubcopilot.com
  transforms:
    - ops:
        - op: remove
          path: $.temperature
"#;
        let cfg = AppConfig::from_yaml_str(yaml).expect("config parse should succeed");
        let settings = cfg.proxy_settings.expect("proxy settings present");
        assert_eq!(
            settings.upstream_base.as_deref(),
            Some("https://api.githubcopilot.com")
        );
        assert_eq!(settings.transforms.len(), 1);
    }
}
