use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TransformRule {
    #[serde(default)]
    pub when: Option<TransformWhen>,
    pub ops: Vec<TransformOp>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TransformWhen {
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum TransformOp {
    Remove { path: String },
    Add { path: String, value: Value },
    Replace { path: String, value: Value },
}

#[derive(Debug, Clone, Copy)]
pub struct RequestContext<'a> {
    pub route: &'a str,
    pub model: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathToken {
    Key(String),
    Index(usize),
}

pub fn apply_transforms(body: &mut Value, rules: &[TransformRule], ctx: RequestContext<'_>) {
    for rule in rules {
        if !rule_matches(rule, ctx) {
            continue;
        }

        for op in &rule.ops {
            match op {
                TransformOp::Remove { path } => remove_path(body, path),
                TransformOp::Add { path, value } => set_path(body, path, value.clone(), false),
                TransformOp::Replace { path, value } => set_path(body, path, value.clone(), true),
            }
        }
    }
}

fn rule_matches(rule: &TransformRule, ctx: RequestContext<'_>) -> bool {
    let Some(when) = &rule.when else {
        return true;
    };

    let route_ok = when.route.as_deref().is_none_or(|r| r == ctx.route);
    let model_ok = when
        .model
        .as_deref()
        .is_none_or(|m| Some(m) == ctx.model);
    route_ok && model_ok
}

fn parse_path(path: &str) -> Option<Vec<PathToken>> {
    if !path.starts_with('$') {
        return None;
    }

    let chars: Vec<char> = path.chars().collect();
    let mut i = 1usize;
    let mut tokens = Vec::new();

    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                if start == i {
                    return None;
                }
                tokens.push(PathToken::Key(path[start..i].to_string()));
            }
            '[' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start || i >= chars.len() || chars[i] != ']' {
                    return None;
                }
                let idx = path[start..i].parse::<usize>().ok()?;
                tokens.push(PathToken::Index(idx));
                i += 1;
            }
            _ => return None,
        }
    }
    Some(tokens)
}

fn remove_path(body: &mut Value, path: &str) {
    let Some(tokens) = parse_path(path) else {
        return;
    };
    if tokens.is_empty() {
        return;
    }

    let (parent_tokens, last) = tokens.split_at(tokens.len() - 1);
    if let Some(parent) = navigate_mut(body, parent_tokens, false, None) {
        match (&last[0], parent) {
            (PathToken::Key(key), Value::Object(map)) => {
                map.remove(key);
            }
            (PathToken::Index(idx), Value::Array(arr)) => {
                if *idx < arr.len() {
                    arr.remove(*idx);
                }
            }
            _ => {}
        }
    }
}

fn set_path(body: &mut Value, path: &str, value: Value, replace_only: bool) {
    let Some(tokens) = parse_path(path) else {
        return;
    };
    if tokens.is_empty() {
        *body = value;
        return;
    }

    let (parent_tokens, last) = tokens.split_at(tokens.len() - 1);
    let Some(parent) = navigate_mut(body, parent_tokens, true, Some(&last[0])) else {
        return;
    };

    match (&last[0], parent) {
        (PathToken::Key(key), Value::Object(map)) => {
            if !replace_only || map.contains_key(key) {
                map.insert(key.clone(), value);
            } else {
                map.insert(key.clone(), value);
            }
        }
        (PathToken::Index(idx), Value::Array(arr)) => {
            let idx = *idx;
            if idx >= arr.len() {
                arr.resize(idx + 1, Value::Null);
            }
            arr[idx] = value;
        }
        _ => {}
    }
}

fn navigate_mut<'a>(
    mut current: &'a mut Value,
    tokens: &[PathToken],
    create_missing: bool,
    terminal_next: Option<&PathToken>,
) -> Option<&'a mut Value> {
    for (pos, token) in tokens.iter().enumerate() {
        let next = tokens.get(pos + 1).or(terminal_next);
        match token {
            PathToken::Key(key) => {
                if !current.is_object() {
                    if !create_missing {
                        return None;
                    }
                    *current = Value::Object(Default::default());
                }

                let map = current.as_object_mut()?;
                if !map.contains_key(key) {
                    if !create_missing {
                        return None;
                    }
                    map.insert(key.clone(), container_for(next));
                }
                current = map.get_mut(key)?;
            }
            PathToken::Index(idx) => {
                if !current.is_array() {
                    if !create_missing {
                        return None;
                    }
                    *current = Value::Array(Vec::new());
                }

                let arr = current.as_array_mut()?;
                if *idx >= arr.len() {
                    if !create_missing {
                        return None;
                    }
                    arr.resize(idx + 1, Value::Null);
                }
                if arr[*idx].is_null() && create_missing {
                    arr[*idx] = container_for(next);
                }
                current = arr.get_mut(*idx)?;
            }
        }
    }

    Some(current)
}

fn container_for(next: Option<&PathToken>) -> Value {
    match next {
        Some(PathToken::Index(_)) => Value::Array(Vec::new()),
        _ => Value::Object(Default::default()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{RequestContext, TransformOp, TransformRule, TransformWhen, apply_transforms};

    #[test]
    fn removes_and_replaces_fields() {
        let mut body = json!({
            "model": "gpt-4o",
            "temperature": 0.8,
            "metadata": {"source": "old"}
        });
        let rules = vec![TransformRule {
            when: None,
            ops: vec![
                TransformOp::Remove {
                    path: "$.temperature".to_string(),
                },
                TransformOp::Replace {
                    path: "$.metadata.source".to_string(),
                    value: json!("proxy"),
                },
            ],
        }];

        apply_transforms(
            &mut body,
            &rules,
            RequestContext {
                route: "/v1/chat/completions",
                model: Some("gpt-4o"),
            },
        );

        assert!(body.get("temperature").is_none());
        assert_eq!(body["metadata"]["source"], json!("proxy"));
    }

    #[test]
    fn adds_nested_field_with_create_missing() {
        let mut body = json!({"model": "gpt-5-mini"});
        let rules = vec![TransformRule {
            when: None,
            ops: vec![TransformOp::Add {
                path: "$.metadata.tags[0]".to_string(),
                value: json!("from-proxy"),
            }],
        }];

        apply_transforms(
            &mut body,
            &rules,
            RequestContext {
                route: "/v1/responses",
                model: Some("gpt-5-mini"),
            },
        );

        assert_eq!(body["metadata"]["tags"][0], json!("from-proxy"));
    }

    #[test]
    fn applies_rules_only_when_route_and_model_match() {
        let mut body = json!({"model": "gpt-4o", "stream": true});
        let rules = vec![TransformRule {
            when: Some(TransformWhen {
                route: Some("/v1/responses".to_string()),
                model: Some("gpt-5-mini".to_string()),
            }),
            ops: vec![TransformOp::Replace {
                path: "$.stream".to_string(),
                value: json!(false),
            }],
        }];

        apply_transforms(
            &mut body,
            &rules,
            RequestContext {
                route: "/v1/chat/completions",
                model: Some("gpt-4o"),
            },
        );

        assert_eq!(body["stream"], json!(true));
    }
}
