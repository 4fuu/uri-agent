use super::{CatalogCredential, CatalogModel};
use anyhow::{Context, Result, anyhow, bail};
use http::header::{ACCEPT, AUTHORIZATION};
use reqwest::{Client, RequestBuilder, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const REFRESH_INTERVAL_MS: i64 = 5 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiscoveryKind {
    OpenAi,
    Anthropic,
    Gemini,
    OpenCode,
}

const PROVIDERS: &[(&str, DiscoveryKind)] = &[
    ("ant-ling", DiscoveryKind::OpenAi),
    ("anthropic", DiscoveryKind::Anthropic),
    ("baseten", DiscoveryKind::OpenAi),
    ("cerebras", DiscoveryKind::OpenAi),
    ("deepseek", DiscoveryKind::OpenAi),
    ("google", DiscoveryKind::Gemini),
    ("groq", DiscoveryKind::OpenAi),
    ("huggingface", DiscoveryKind::OpenAi),
    ("minimax", DiscoveryKind::Anthropic),
    ("minimax-cn", DiscoveryKind::Anthropic),
    ("moonshotai", DiscoveryKind::OpenAi),
    ("moonshotai-cn", DiscoveryKind::OpenAi),
    ("nvidia", DiscoveryKind::OpenAi),
    ("openai", DiscoveryKind::OpenAi),
    ("opencode", DiscoveryKind::OpenCode),
    ("opencode-go", DiscoveryKind::OpenCode),
    ("openrouter", DiscoveryKind::OpenAi),
    ("qwen-token-plan", DiscoveryKind::OpenAi),
    ("qwen-token-plan-cn", DiscoveryKind::OpenAi),
    ("qwen-token-plan-individual", DiscoveryKind::OpenAi),
    ("together", DiscoveryKind::OpenAi),
    ("xai", DiscoveryKind::OpenAi),
    ("xiaomi", DiscoveryKind::OpenAi),
    ("xiaomi-token-plan-ams", DiscoveryKind::OpenAi),
    ("xiaomi-token-plan-cn", DiscoveryKind::OpenAi),
    ("xiaomi-token-plan-sgp", DiscoveryKind::OpenAi),
    ("zai", DiscoveryKind::OpenAi),
    ("zai-coding-cn", DiscoveryKind::OpenAi),
];

#[derive(Clone)]
struct DiscoveredModel {
    id: String,
    name: Option<String>,
    raw: Value,
}

pub(super) fn supports_provider(provider: &str) -> bool {
    discovery_kind(provider).is_some()
}

#[cfg(test)]
pub(super) fn provider_ids() -> impl Iterator<Item = &'static str> {
    PROVIDERS.iter().map(|(provider, _)| *provider)
}

pub(super) fn credential_fingerprint(provider: &str, credential: &CatalogCredential) -> String {
    let mut hash = Sha256::new();
    hash.update(b"uri-agent-model-discovery\0");
    hash.update(provider.as_bytes());
    hash.update([u8::from(credential.oauth)]);
    hash.update(credential.secret.as_bytes());
    format!("sha256-{:x}", hash.finalize())
}

pub(super) async fn discover(
    client: &Client,
    provider: &str,
    credential: &CatalogCredential,
    catalog: &BTreeMap<String, Vec<CatalogModel>>,
) -> Result<Vec<CatalogModel>> {
    let kind = discovery_kind(provider)
        .ok_or_else(|| anyhow!("provider {provider} has no model discovery contract"))?;
    let references = catalog
        .get(provider)
        .ok_or_else(|| anyhow!("provider {provider} has no catalog models"))?;
    let endpoint = discovery_endpoint(provider, kind, references)?;
    let records = match kind {
        DiscoveryKind::OpenAi | DiscoveryKind::OpenCode => {
            fetch_openai(client, endpoint, credential).await?
        }
        DiscoveryKind::Anthropic => fetch_anthropic(client, endpoint, credential).await?,
        DiscoveryKind::Gemini => fetch_gemini(client, endpoint, credential).await?,
    };
    Ok(materialize(provider, kind, records, catalog))
}

fn discovery_kind(provider: &str) -> Option<DiscoveryKind> {
    PROVIDERS
        .iter()
        .find_map(|(candidate, kind)| (*candidate == provider).then_some(*kind))
}

fn discovery_endpoint(
    provider: &str,
    kind: DiscoveryKind,
    references: &[CatalogModel],
) -> Result<Url> {
    let base_url = match kind {
        DiscoveryKind::OpenCode => references
            .iter()
            .find(|model| model.base_url.trim_end_matches('/').ends_with("/v1"))
            .or_else(|| references.first()),
        _ => references.first(),
    }
    .map(|model| model.base_url.trim_end_matches('/'))
    .filter(|base_url| !base_url.is_empty())
    .ok_or_else(|| anyhow!("provider {provider} has no discovery base URL"))?;
    let endpoint = match kind {
        DiscoveryKind::OpenAi | DiscoveryKind::OpenCode | DiscoveryKind::Gemini => {
            format!("{base_url}/models")
        }
        DiscoveryKind::Anthropic => {
            if base_url.ends_with("/v1") {
                format!("{base_url}/models")
            } else {
                format!("{base_url}/v1/models")
            }
        }
    };
    Url::parse(&endpoint).with_context(|| format!("invalid discovery URL for {provider}"))
}

async fn fetch_openai(
    client: &Client,
    endpoint: Url,
    credential: &CatalogCredential,
) -> Result<Vec<DiscoveredModel>> {
    let value = send_json(bearer_request(client.get(endpoint), credential)).await?;
    parse_openai_models(value)
}

async fn fetch_anthropic(
    client: &Client,
    mut endpoint: Url,
    credential: &CatalogCredential,
) -> Result<Vec<DiscoveredModel>> {
    let mut models = Vec::new();
    let mut after_id = None;
    let mut cursors = BTreeSet::new();
    loop {
        endpoint
            .query_pairs_mut()
            .clear()
            .append_pair("limit", "1000");
        if let Some(cursor) = after_id.as_deref() {
            endpoint.query_pairs_mut().append_pair("after_id", cursor);
        }
        let mut request = client
            .get(endpoint.clone())
            .header(ACCEPT, "application/json")
            .header("anthropic-version", "2023-06-01");
        request = if credential.oauth {
            bearer_request(request, credential)
        } else {
            request.header("x-api-key", &credential.secret)
        };
        let value = send_json(request).await?;
        let has_more = value
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let next = value
            .get("last_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        models.extend(parse_openai_models(value)?);
        if !has_more {
            break;
        }
        let next = next.ok_or_else(|| anyhow!("model listing omitted its next cursor"))?;
        if !cursors.insert(next.clone()) {
            bail!("model listing repeated its pagination cursor");
        }
        after_id = Some(next);
    }
    Ok(deduplicate(models))
}

async fn fetch_gemini(
    client: &Client,
    mut endpoint: Url,
    credential: &CatalogCredential,
) -> Result<Vec<DiscoveredModel>> {
    let mut models = Vec::new();
    let mut page_token = None;
    let mut cursors = BTreeSet::new();
    loop {
        endpoint
            .query_pairs_mut()
            .clear()
            .append_pair("pageSize", "1000");
        if let Some(cursor) = page_token.as_deref() {
            endpoint.query_pairs_mut().append_pair("pageToken", cursor);
        }
        let value = send_json(
            client
                .get(endpoint.clone())
                .header(ACCEPT, "application/json")
                .header("x-goog-api-key", &credential.secret),
        )
        .await?;
        let records = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Gemini model listing has no models array"))?;
        for raw in records {
            let supports_generation = raw
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .is_some_and(|methods| methods.iter().any(|method| method == "generateContent"));
            if !supports_generation {
                continue;
            }
            let Some(id) = raw
                .get("name")
                .and_then(Value::as_str)
                .and_then(|name| name.strip_prefix("models/"))
                .filter(|id| !id.trim().is_empty())
            else {
                continue;
            };
            models.push(DiscoveredModel {
                id: id.to_string(),
                name: raw
                    .get("displayName")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                raw: raw.clone(),
            });
        }
        let next = value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string);
        let Some(next) = next else {
            break;
        };
        if !cursors.insert(next.clone()) {
            bail!("Gemini model listing repeated its pagination cursor");
        }
        page_token = Some(next);
    }
    Ok(deduplicate(models))
}

fn bearer_request(request: RequestBuilder, credential: &CatalogCredential) -> RequestBuilder {
    request
        .header(ACCEPT, "application/json")
        .header(AUTHORIZATION, format!("Bearer {}", credential.secret))
}

async fn send_json(request: RequestBuilder) -> Result<Value> {
    request
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await
        .context("cannot parse provider model listing")
}

fn parse_openai_models(value: Value) -> Result<Vec<DiscoveredModel>> {
    let values = match value {
        Value::Array(values) => values,
        Value::Object(mut object) => {
            let array = ["data", "models", "result", "items"]
                .into_iter()
                .find_map(|key| object.remove(key));
            match array {
                Some(Value::Array(values)) => values,
                Some(_) => bail!("provider model listing field is not an array"),
                None if object.values().all(Value::is_object) => object.into_values().collect(),
                None => bail!("provider model listing has no model array"),
            }
        }
        _ => bail!("provider model listing is not an object or array"),
    };
    let models = values
        .into_iter()
        .filter_map(|raw| {
            let id = raw
                .get("id")
                .or_else(|| raw.get("name"))
                .and_then(Value::as_str)?
                .trim();
            if id.is_empty() {
                return None;
            }
            let name = raw
                .get("display_name")
                .or_else(|| raw.get("displayName"))
                .or_else(|| raw.get("name"))
                .and_then(Value::as_str)
                .filter(|name| *name != id)
                .map(str::to_string);
            Some(DiscoveredModel {
                id: id.to_string(),
                name,
                raw,
            })
        })
        .collect();
    Ok(deduplicate(models))
}

fn deduplicate(models: Vec<DiscoveredModel>) -> Vec<DiscoveredModel> {
    models
        .into_iter()
        .map(|model| (model.id.clone(), model))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

fn materialize(
    provider: &str,
    kind: DiscoveryKind,
    records: Vec<DiscoveredModel>,
    catalog: &BTreeMap<String, Vec<CatalogModel>>,
) -> Vec<CatalogModel> {
    let references = &catalog[provider];
    let existing = references
        .iter()
        .map(|model| model.id.as_str())
        .collect::<BTreeSet<_>>();
    records
        .into_iter()
        .filter(|record| !existing.contains(record.id.as_str()))
        .filter(|record| is_generation_model(provider, record))
        .filter_map(|record| materialize_one(provider, kind, record, references, catalog))
        .collect()
}

fn materialize_one(
    provider: &str,
    kind: DiscoveryKind,
    record: DiscoveredModel,
    references: &[CatalogModel],
    catalog: &BTreeMap<String, Vec<CatalogModel>>,
) -> Option<CatalogModel> {
    let prefixed = prefix_template(&record.id, references);
    let api = match kind {
        DiscoveryKind::OpenCode => prefixed
            .map(|model| model.api.as_str())
            .unwrap_or_else(|| open_code_api(provider, &record.id)),
        _ => single_supported_api(references)?,
    };
    let template = catalog
        .values()
        .flatten()
        .find(|model| model.id == record.id && model.api == api)
        .or(prefixed.filter(|model| model.api == api))
        .or_else(|| closest_template(&record.id, api, references))
        .or_else(|| references.iter().find(|model| model.api == api))?;
    let base_url = references
        .iter()
        .find(|model| model.api == api)
        .map(|model| model.base_url.clone())?;
    let mut model = template.clone();
    model.id.clone_from(&record.id);
    model.provider = provider.to_string();
    model.api = api.to_string();
    model.base_url = base_url;
    model.name = record
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| discovered_name(template, &record.id));
    model.metadata.remove("cost");
    model.metadata.remove("hidden");
    model
        .metadata
        .insert("discovered".to_string(), Value::Bool(true));
    model.metadata.insert(
        "metadataSourceModel".to_string(),
        Value::String(format!("{}/{}", template.provider, template.id)),
    );
    Some(model)
}

fn single_supported_api(references: &[CatalogModel]) -> Option<&str> {
    let apis = references
        .iter()
        .filter(|model| model.supported())
        .map(|model| model.api.as_str())
        .collect::<BTreeSet<_>>();
    (apis.len() == 1).then(|| *apis.first().expect("one API is present"))
}

fn prefix_template<'a>(id: &str, references: &'a [CatalogModel]) -> Option<&'a CatalogModel> {
    let id = id.to_ascii_lowercase();
    references
        .iter()
        .filter(|model| {
            let candidate = model.id.to_ascii_lowercase();
            id.strip_prefix(&candidate)
                .is_some_and(|suffix| suffix.starts_with(['-', '.', ':']))
        })
        .max_by_key(|model| model.id.len())
}

fn closest_template<'a>(
    id: &str,
    api: &str,
    references: &'a [CatalogModel],
) -> Option<&'a CatalogModel> {
    let root = model_root(id);
    references
        .iter()
        .filter(|model| model.api == api && model_root(&model.id) == root)
        .max_by_key(|model| common_prefix_length(id, &model.id))
}

fn model_root(id: &str) -> &str {
    let id = id.rsplit('/').next().unwrap_or(id);
    id.split(['-', '.', ':']).next().unwrap_or(id)
}

fn common_prefix_length(left: &str, right: &str) -> usize {
    left.bytes()
        .zip(right.bytes())
        .take_while(|(left, right)| left.eq_ignore_ascii_case(right))
        .count()
}

fn open_code_api(provider: &str, id: &str) -> &'static str {
    let id = id.to_ascii_lowercase();
    if id.starts_with("claude-") || (provider == "opencode" && id.starts_with("qwen")) {
        "anthropic-messages"
    } else if id.starts_with("gemini-") {
        "google-generative-ai"
    } else if ["gpt-", "grok-", "muse-", "mai-code-"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        "openai-responses"
    } else {
        "openai-completions"
    }
}

fn discovered_name(template: &CatalogModel, id: &str) -> String {
    if let Some(suffix) = id.strip_prefix(&template.id)
        && let Some(suffix) = suffix.strip_prefix('-')
        && !suffix.is_empty()
    {
        return format!("{} {}", template.name, title_suffix(suffix));
    }
    id.to_string()
}

fn title_suffix(suffix: &str) -> String {
    suffix
        .split('-')
        .map(|part| {
            let mut characters = part.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(characters).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_generation_model(provider: &str, model: &DiscoveredModel) -> bool {
    let id = model.id.to_ascii_lowercase();
    if [
        "embedding",
        "embed-",
        "rerank",
        "moderation",
        "dall-e",
        "whisper",
        "transcri",
        "tts",
        "speech",
        "realtime",
    ]
    .iter()
    .any(|part| id.contains(part))
    {
        return false;
    }
    if provider == "openrouter"
        && let Some(parameters) = model
            .raw
            .get("supported_parameters")
            .and_then(Value::as_array)
    {
        return parameters.iter().any(|parameter| parameter == "tools");
    }
    if provider == "together"
        && let Some(kind) = model.raw.get("type").and_then(Value::as_str)
    {
        return matches!(kind, "chat" | "language" | "text");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn model_server(
        bodies: Vec<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "client closed before sending HTTP headers");
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), server)
    }

    fn model(provider: &str, id: &str, name: &str, api: &str, base_url: &str) -> CatalogModel {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": name,
            "api": api,
            "provider": provider,
            "baseUrl": base_url,
            "reasoning": true,
            "input": ["text"],
            "contextWindow": 1000000,
            "maxTokens": 131072,
            "cost": {"input": 1.4, "output": 4.4, "cacheRead": 0.26, "cacheWrite": 0},
            "thinkingLevelMap": {"off": null, "low": "low", "high": "high", "max": "max"}
        }))
        .unwrap()
    }

    #[test]
    fn discovery_is_limited_to_supported_provider_contracts() {
        assert_eq!(provider_ids().count(), 28);
        assert!(supports_provider("opencode-go"));
        assert!(supports_provider("google"));
        assert!(!supports_provider("amazon-bedrock"));
        assert!(!supports_provider("openai-codex"));
    }

    #[test]
    fn opencode_unknown_glm_inherits_the_nearest_model_without_copying_price() {
        let reference = model(
            "opencode-go",
            "glm-5.3",
            "GLM-5.3",
            "openai-completions",
            "https://opencode.test/zen/go/v1",
        );
        let catalog = BTreeMap::from([("opencode-go".to_string(), vec![reference])]);
        let records = vec![DiscoveredModel {
            id: "glm-5.3-flash".to_string(),
            name: None,
            raw: serde_json::json!({"id": "glm-5.3-flash"}),
        }];
        let discovered = materialize("opencode-go", DiscoveryKind::OpenCode, records, &catalog);
        assert_eq!(discovered.len(), 1);
        let model = &discovered[0];
        assert_eq!(model.id, "glm-5.3-flash");
        assert_eq!(model.name, "GLM-5.3 Flash");
        assert_eq!(model.api, "openai-completions");
        assert_eq!(model.base_url, "https://opencode.test/zen/go/v1");
        assert_eq!(model.context_window(), 1_000_000);
        assert!(model.reasoning());
        assert!(!model.metadata.contains_key("cost"));
        assert_eq!(model.metadata["metadataSourceModel"], "opencode-go/glm-5.3");
    }

    #[test]
    fn zai_unknown_glm_flash_is_materialized_for_its_supported_api() {
        let reference = model(
            "zai",
            "glm-5.3",
            "GLM-5.3",
            "openai-completions",
            "https://api.z.ai/api/coding/paas/v4",
        );
        let catalog = BTreeMap::from([("zai".to_string(), vec![reference])]);
        let records = vec![DiscoveredModel {
            id: "glm-5.3-flash".to_string(),
            name: Some("GLM-5.3-Flash".to_string()),
            raw: serde_json::json!({"id": "glm-5.3-flash"}),
        }];

        let discovered = materialize("zai", DiscoveryKind::OpenAi, records, &catalog);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, "glm-5.3-flash");
        assert_eq!(discovered[0].name, "GLM-5.3-Flash");
        assert_eq!(discovered[0].api, "openai-completions");
        assert_eq!(
            discovered[0].base_url,
            "https://api.z.ai/api/coding/paas/v4"
        );
        assert!(!discovered[0].metadata.contains_key("cost"));
    }

    #[test]
    fn openai_parser_accepts_common_shapes_and_filters_non_generation_models() {
        let parsed = parse_openai_models(serde_json::json!({
            "data": [
                {"id": "gpt-new", "display_name": "GPT New"},
                {"id": "text-embedding-new"}
            ]
        }))
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(is_generation_model("openai", &parsed[0]));
        assert!(!is_generation_model("openai", &parsed[1]));
    }

    #[test]
    fn open_code_fallback_selects_only_locally_supported_apis() {
        assert_eq!(
            open_code_api("opencode", "claude-new"),
            "anthropic-messages"
        );
        assert_eq!(
            open_code_api("opencode", "gemini-new"),
            "google-generative-ai"
        );
        assert_eq!(open_code_api("opencode-go", "gpt-new"), "openai-responses");
        assert_eq!(
            open_code_api("opencode-go", "glm-new"),
            "openai-completions"
        );
    }

    #[tokio::test]
    async fn opencode_discovery_uses_bearer_auth_and_adds_unknown_models() {
        let (endpoint, server) = model_server(vec![
            r#"{"data":[{"id":"glm-5.3"},{"id":"glm-5.3-flash"}]}"#,
        ])
        .await;
        let reference = model(
            "opencode-go",
            "glm-5.3",
            "GLM-5.3",
            "openai-completions",
            &format!("{endpoint}/v1"),
        );
        let catalog = BTreeMap::from([("opencode-go".to_string(), vec![reference])]);

        let discovered = discover(
            &Client::new(),
            "opencode-go",
            &CatalogCredential {
                secret: "go-test-key".to_string(),
                oauth: false,
            },
            &catalog,
        )
        .await
        .unwrap();
        let requests = server.await.unwrap();

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, "glm-5.3-flash");
        assert_eq!(discovered[0].api, "openai-completions");
        assert!(requests[0].starts_with("GET /v1/models HTTP/1.1\r\n"));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer go-test-key\r\n")
        );
    }

    #[tokio::test]
    async fn anthropic_discovery_paginates_with_native_headers() {
        let (endpoint, server) = model_server(vec![
            r#"{"data":[{"id":"claude-existing"}],"has_more":true,"last_id":"page-one"}"#,
            r#"{"data":[{"id":"claude-new"}],"has_more":false}"#,
        ])
        .await;
        let reference = model(
            "anthropic",
            "claude-existing",
            "Claude Existing",
            "anthropic-messages",
            &format!("{endpoint}/v1"),
        );
        let catalog = BTreeMap::from([("anthropic".to_string(), vec![reference])]);

        let discovered = discover(
            &Client::new(),
            "anthropic",
            &CatalogCredential {
                secret: "anthropic-test-key".to_string(),
                oauth: false,
            },
            &catalog,
        )
        .await
        .unwrap();
        let requests = server.await.unwrap();

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, "claude-new");
        assert!(requests[0].starts_with("GET /v1/models?limit=1000 HTTP/1.1\r\n"));
        assert!(
            requests[1].starts_with("GET /v1/models?limit=1000&after_id=page-one HTTP/1.1\r\n")
        );
        for request in requests {
            let request = request.to_ascii_lowercase();
            assert!(request.contains("x-api-key: anthropic-test-key\r\n"));
            assert!(request.contains("anthropic-version: 2023-06-01\r\n"));
        }
    }

    #[tokio::test]
    async fn gemini_discovery_paginates_and_filters_non_generation_models() {
        let (endpoint, server) = model_server(vec![
            r#"{"models":[{"name":"models/gemini-existing","supportedGenerationMethods":["generateContent"]},{"name":"models/text-embedding-new","supportedGenerationMethods":["embedContent"]}],"nextPageToken":"page-two"}"#,
            r#"{"models":[{"name":"models/gemini-new","displayName":"Gemini New","supportedGenerationMethods":["generateContent"]}]}"#,
        ])
        .await;
        let reference = model(
            "google",
            "gemini-existing",
            "Gemini Existing",
            "google-generative-ai",
            &format!("{endpoint}/v1beta"),
        );
        let catalog = BTreeMap::from([("google".to_string(), vec![reference])]);

        let discovered = discover(
            &Client::new(),
            "google",
            &CatalogCredential {
                secret: "google-test-key".to_string(),
                oauth: false,
            },
            &catalog,
        )
        .await
        .unwrap();
        let requests = server.await.unwrap();

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, "gemini-new");
        assert_eq!(discovered[0].name, "Gemini New");
        assert!(requests[0].starts_with("GET /v1beta/models?pageSize=1000 HTTP/1.1\r\n"));
        assert!(
            requests[1]
                .starts_with("GET /v1beta/models?pageSize=1000&pageToken=page-two HTTP/1.1\r\n")
        );
        for request in requests {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("x-goog-api-key: google-test-key\r\n")
            );
        }
    }
}
