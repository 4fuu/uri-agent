//! WorkBuddy cloud product configuration model discovery.

use super::{CatalogCredential, CatalogModel};
use crate::oauth::workbuddy_authenticated_headers;
use anyhow::{Context, Result, anyhow, bail};
use http::header::{ACCEPT, CONNECTION};
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub(crate) const PROVIDER: &str = "workbuddy";
pub(crate) const REFRESH_INTERVAL_MS: i64 = 8 * 60 * 1000;
pub(crate) const MAX_CACHED_CONFIGS: usize = 20;

pub(crate) fn remote_config_enabled() -> bool {
    !std::env::var("CODEBUDDY_REMOTE_CONFIG_DISABLED")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true"))
}

pub(crate) async fn discover(
    client: &Client,
    credential: &CatalogCredential,
) -> Result<Vec<CatalogModel>> {
    let context = credential
        .workbuddy
        .as_ref()
        .ok_or_else(|| anyhow!("WorkBuddy model discovery has no session context"))?;
    let endpoint = context.session.endpoint.trim_end_matches('/');
    let endpoint = endpoint.strip_suffix("/v2").unwrap_or(endpoint);
    let url = Url::parse(&format!("{endpoint}/v3/config"))
        .context("invalid WorkBuddy cloud product configuration URL")?;
    let mut headers =
        workbuddy_authenticated_headers(&credential.secret, context.api_key, &context.session)?;
    headers.insert(CONNECTION, "close".parse().expect("static header value"));
    headers.insert(
        ACCEPT,
        "application/json".parse().expect("static header value"),
    );

    let response = client
        .get(url)
        .headers(headers)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?;
    let value = response.json::<Value>().await?;
    if value
        .get("code")
        .and_then(code_value)
        .is_some_and(|code| code != 0)
    {
        bail!("WorkBuddy cloud product configuration was rejected");
    }
    parse_product_config(value, endpoint)
}

fn parse_product_config(value: Value, endpoint: &str) -> Result<Vec<CatalogModel>> {
    let product = value
        .get("data")
        .and_then(|value| value.get("data"))
        .or_else(|| value.get("data"))
        .unwrap_or(&value);
    let records = product
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("WorkBuddy cloud product configuration has no models array"))?;
    let models = records
        .iter()
        .filter_map(|record| catalog_model(record, endpoint))
        .collect::<Vec<_>>();
    if models.is_empty() {
        bail!("WorkBuddy cloud product configuration has no runnable chat models");
    }
    Ok(models)
}

fn catalog_model(raw: &Value, endpoint: &str) -> Option<CatalogModel> {
    let id = raw.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let context_window = raw
        .get("maxInputTokens")?
        .as_u64()
        .filter(|value| *value > 0)?;
    let max_tokens = raw
        .get("maxOutputTokens")?
        .as_u64()
        .filter(|value| *value > 0)?;
    if raw.get("supportsToolCall").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let supports_reasoning = raw
        .get("supportsReasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let supports_images = raw
        .get("supportsImages")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut sampling = Map::new();
    for key in ["temperature", "top_p"] {
        if let Some(value) = raw.get(key).filter(|value| value.is_number()) {
            sampling.insert(key.to_string(), value.clone());
        }
    }
    let base_url = format!("{}/v2", endpoint.trim_end_matches('/'));
    let metadata = BTreeMap::from([
        ("reasoning".to_string(), Value::Bool(supports_reasoning)),
        (
            "input".to_string(),
            if supports_images {
                json!(["text", "image"])
            } else {
                json!(["text"])
            },
        ),
        (
            "cost".to_string(),
            json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}),
        ),
        ("contextWindow".to_string(), Value::from(context_window)),
        ("maxTokens".to_string(), Value::from(max_tokens)),
        (
            "thinkingLevelMap".to_string(),
            thinking_level_map(raw, supports_reasoning),
        ),
        ("samplingParams".to_string(), Value::Object(sampling)),
        (
            "compat".to_string(),
            json!({
                "maxTokensField": "max_tokens",
                "supportsStore": false,
                "supportsDeveloperRole": false,
                "supportsStrictMode": false,
                "supportsReasoningEffort": true,
                "sendReasoningEffortWhenOff": false,
                "reasoningSummary": "auto"
            }),
        ),
        ("discovered".to_string(), Value::Bool(true)),
        ("workbuddy".to_string(), raw.clone()),
    ]);
    Some(CatalogModel {
        id: id.to_string(),
        name: raw
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(id)
            .to_string(),
        api: "openai-completions".to_string(),
        provider: PROVIDER.to_string(),
        base_url,
        headers: BTreeMap::new(),
        metadata,
    })
}

fn thinking_level_map(raw: &Value, supports_reasoning: bool) -> Value {
    if !supports_reasoning {
        return json!({});
    }
    let reasoning = raw.get("reasoning");
    let only_reasoning = raw
        .get("onlyReasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let can_disable = reasoning
        .and_then(|value| value.get("canDisableThinking"))
        .and_then(Value::as_bool)
        .unwrap_or(!only_reasoning);
    let mut levels = Map::from_iter([(
        "off".to_string(),
        if can_disable {
            Value::String("off".to_string())
        } else {
            Value::Null
        },
    )]);
    let efforts = reasoning
        .and_then(|value| value.get("supportedEfforts"))
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .filter(|values| !values.is_empty())
        .or_else(|| {
            reasoning
                .and_then(|value| value.get("effort"))
                .and_then(Value::as_str)
                .map(|effort| vec![effort])
        })
        .unwrap_or_else(|| vec!["low", "medium", "high"]);
    for effort in efforts {
        levels.insert(effort.to_string(), Value::String(effort.to_string()));
    }
    Value::Object(levels)
}

fn code_value(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::WorkBuddyCatalogCredential;
    use crate::oauth::WorkBuddySession;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn cloud_models_are_converted_and_media_generators_are_filtered() {
        let models = parse_product_config(
            json!({
                "code": 0,
                "data": {"data": {"models": [
                    {
                        "id": "gpt-cloud",
                        "name": "GPT Cloud",
                        "maxInputTokens": 200000,
                        "maxOutputTokens": 32000,
                        "supportsToolCall": true,
                        "supportsImages": true,
                        "supportsReasoning": true,
                        "onlyReasoning": true,
                        "reasoning": {
                            "supportedEfforts": ["medium", "high"],
                            "canDisableThinking": false
                        },
                        "temperature": 1
                    },
                    {"id": "image-only", "name": "Image only"}
                ]}}
            }),
            "https://copilot.tencent.com",
        )
        .unwrap();

        assert_eq!(models.len(), 1);
        let model = &models[0];
        assert_eq!(model.id, "gpt-cloud");
        assert_eq!(model.base_url, "https://copilot.tencent.com/v2");
        assert_eq!(model.context_window(), 200_000);
        assert_eq!(model.max_tokens(), 32_000);
        assert_eq!(model.metadata["input"], json!(["text", "image"]));
        assert_eq!(model.metadata["thinkingLevelMap"]["off"], Value::Null);
        assert_eq!(model.metadata["thinkingLevelMap"]["high"], "high");
        assert_eq!(model.metadata["samplingParams"]["temperature"], 1);
        assert_eq!(model.metadata["workbuddy"]["name"], "GPT Cloud");
    }

    #[test]
    fn cloud_payload_without_runnable_models_is_not_a_last_good_config() {
        assert!(
            parse_product_config(
                json!({"data": {"data": {"models": [{"id": "image-only"}]}}}),
                "https://example.com"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn cloud_fetch_uses_the_authenticated_product_config_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let body = json!({
                "code": 0,
                "data": {"data": {"models": [{
                    "id": "remote-chat",
                    "name": "Remote Chat",
                    "maxInputTokens": 128000,
                    "maxOutputTokens": 16000,
                    "supportsToolCall": true
                }]}}
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        let credential = CatalogCredential {
            secret: "oauth-access".to_string(),
            oauth: true,
            workbuddy: Some(WorkBuddyCatalogCredential {
                session: WorkBuddySession {
                    endpoint: format!("http://{address}/v2"),
                    domain: Some("enterprise.example".to_string()),
                    method: Some("github".to_string()),
                    account: Some(json!({
                        "uid": "user@example.com",
                        "enterpriseId": "enterprise-1",
                        "departmentFullName": "engineering",
                        "idSource": "github"
                    })),
                },
                api_key: false,
            }),
        };

        let models = discover(&Client::new(), &credential).await.unwrap();
        let request = server.await.unwrap();
        let lower = request.to_ascii_lowercase();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "remote-chat");
        assert!(request.starts_with("GET /v3/config HTTP/1.1\r\n"));
        assert!(lower.contains("authorization: bearer oauth-access\r\n"));
        assert!(lower.contains("x-requested-with: xmlhttprequest\r\n"));
        assert!(lower.contains("x-product: saas\r\n"));
        assert!(lower.contains(&format!(
            "user-agent: {}\r\n",
            crate::oauth::WORKBUDDY_USER_AGENT.to_ascii_lowercase()
        )));
        assert!(lower.contains("x-domain: enterprise.example\r\n"));
        assert!(lower.contains("x-user-id: user@example.com\r\n"));
        assert!(lower.contains("x-enterprise-id: enterprise-1\r\n"));
        assert!(lower.contains("connection: close\r\n"));
        assert!(!lower.contains("x-api-key:"));
    }
}
