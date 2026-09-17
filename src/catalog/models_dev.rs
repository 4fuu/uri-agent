use super::{read_json, write_json};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) const API_URL: &str = "https://models.dev/api.json";
pub(super) const REFRESH_INTERVAL_MS: i64 = 4 * 60 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// Only OpenCode discovery consults these hints: models.dev records per-model
// protocol overrides (for example union-alpha on anthropic-messages) that the
// Pi catalog has not published yet. Keep this list in sync with the
// DiscoveryKind::OpenCode providers in discovery.rs.
const HINT_PROVIDERS: &[&str] = &["opencode", "opencode-go"];

pub(super) fn uses_hints(provider: &str) -> bool {
    HINT_PROVIDERS.contains(&provider)
}

pub(super) fn cache_path(directory: &Path) -> PathBuf {
    directory.join("models-dev.json")
}

// Hints are advisory: they only select the request API family for discovered
// models and never override Pi catalog records.
fn npm_api(npm: &str) -> Option<&'static str> {
    match npm {
        "@ai-sdk/anthropic" => Some("anthropic-messages"),
        "@ai-sdk/openai" => Some("openai-responses"),
        "@ai-sdk/openai-compatible" => Some("openai-completions"),
        "@ai-sdk/google" => Some("google-generative-ai"),
        _ => None,
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProtocolHints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    hints: BTreeMap<String, BTreeMap<String, String>>,
}

impl ProtocolHints {
    pub(super) fn api(&self, provider: &str, model: &str) -> Option<&str> {
        self.hints.get(provider)?.get(model).map(String::as_str)
    }

    // An empty hint set never counts as fetched, so a first failed or empty
    // fetch retries on the next catalog refresh instead of blocking for the
    // full interval.
    fn fetched_recently(&self, now: i64) -> bool {
        self.checked_at
            .is_some_and(|checked| now - checked < REFRESH_INTERVAL_MS)
            && !self.hints.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn from_hints(hints: &[(&str, &str, &str)]) -> Self {
        let mut map: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for (provider, model, api) in hints {
            map.entry((*provider).to_string())
                .or_default()
                .insert((*model).to_string(), (*api).to_string());
        }
        Self {
            checked_at: None,
            hints: map,
        }
    }
}

pub(super) async fn load(directory: &Path) -> ProtocolHints {
    read_json(&cache_path(directory)).await.unwrap_or_default()
}

pub(super) async fn refresh(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    force: bool,
    cached: &mut ProtocolHints,
    now: i64,
) -> Result<()> {
    if !force && cached.fetched_recently(now) {
        return Ok(());
    }
    match fetch(client, url).await {
        Ok(hints) => {
            *cached = ProtocolHints {
                checked_at: Some(now),
                hints,
            };
        }
        // A failed refresh keeps the cached hints and suppresses retries for
        // the interval, matching the Pi provider fallback behavior.
        Err(_) => cached.checked_at = Some(now),
    }
    write_json(path, cached).await?;
    Ok(())
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let value = client
        .get(url)
        .header("Accept", "application/json")
        .timeout(REQUEST_TIMEOUT) // api.json is several megabytes
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    Ok(distill(&value))
}

fn distill(value: &Value) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut hints = BTreeMap::new();
    let Some(providers) = value.as_object() else {
        return hints;
    };
    for provider in HINT_PROVIDERS {
        let Some(models) = providers
            .get(*provider)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        let mut overrides = BTreeMap::new();
        for (id, model) in models {
            let Some(api) = model
                .get("provider")
                .and_then(|provider| provider.get("npm"))
                .and_then(Value::as_str)
                .and_then(npm_api)
            else {
                continue;
            };
            overrides.insert(normalized_id(id), api.to_string());
        }
        if !overrides.is_empty() {
            hints.insert((*provider).to_string(), overrides);
        }
    }
    hints
}

fn normalized_id(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn api_fixture() -> Value {
        serde_json::json!({
            "opencode-go": {"models": {
                "union-alpha": {"provider": {"npm": "@ai-sdk/anthropic"}},
                "gpt-5.6-luna": {"provider": {"npm": "@ai-sdk/openai"}},
                "glm-5.3": {"name": "GLM-5.3"},
                "omen-alpha": {"provider": {"npm": "@ai-sdk/mistral"}},
                "opencode-go/qwen3.8-flash": {"provider": {"npm": "@ai-sdk/anthropic"}}
            }},
            "opencode": {"models": {
                "union-alpha": {"provider": {"npm": "@ai-sdk/anthropic"}}
            }},
            "xai": {"models": {
                "grok-4.6": {"provider": {"npm": "@ai-sdk/openai"}}
            }}
        })
    }

    async fn hint_server(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
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
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), server)
    }

    #[test]
    fn distill_keeps_only_opencode_protocol_overrides() {
        let hints = distill(&api_fixture());
        assert_eq!(
            hints["opencode-go"]["union-alpha"], "anthropic-messages",
            "explicit per-model override is kept"
        );
        assert_eq!(
            hints["opencode-go"]["gpt-5.6-luna"], "openai-responses",
            "responses-package override is kept"
        );
        assert_eq!(
            hints["opencode-go"]["qwen3.8-flash"], "anthropic-messages",
            "provider-prefixed ids are normalized"
        );
        assert!(
            !hints["opencode-go"].contains_key("glm-5.3"),
            "models without an explicit package override produce no hint"
        );
        assert!(
            !hints["opencode-go"].contains_key("omen-alpha"),
            "unsupported packages produce no hint"
        );
        assert_eq!(hints["opencode"]["union-alpha"], "anthropic-messages");
        assert!(!hints.contains_key("xai"), "other providers are not cached");
    }

    #[test]
    fn api_lookup_is_scoped_to_provider_and_model() {
        let hints =
            ProtocolHints::from_hints(&[("opencode-go", "union-alpha", "anthropic-messages")]);
        assert_eq!(
            hints.api("opencode-go", "union-alpha"),
            Some("anthropic-messages")
        );
        assert_eq!(hints.api("opencode", "union-alpha"), None);
        assert_eq!(hints.api("opencode-go", "omen-alpha"), None);
        assert_eq!(
            ProtocolHints::default().api("opencode-go", "union-alpha"),
            None
        );
    }

    #[tokio::test]
    async fn refresh_distills_and_round_trips_through_the_cache_file() {
        let root = tempfile::tempdir().unwrap();
        let (url, server) = hint_server(vec![(200, api_fixture().to_string())]).await;
        let mut hints = ProtocolHints::default();

        refresh(
            &reqwest::Client::new(),
            &url,
            &cache_path(root.path()),
            false,
            &mut hints,
            1_000,
        )
        .await
        .unwrap();
        let requests = server.await.unwrap();

        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET / HTTP/1.1\r\n"));
        assert_eq!(
            hints.api("opencode-go", "union-alpha"),
            Some("anthropic-messages")
        );
        let reloaded = load(root.path()).await;
        assert_eq!(
            reloaded.api("opencode-go", "union-alpha"),
            Some("anthropic-messages")
        );
    }

    #[tokio::test]
    async fn failed_refresh_keeps_cached_hints_and_suppresses_retries() {
        let root = tempfile::tempdir().unwrap();
        let (url, server) = hint_server(vec![(500, "{}".to_string())]).await;
        let mut hints =
            ProtocolHints::from_hints(&[("opencode-go", "union-alpha", "anthropic-messages")]);

        refresh(
            &reqwest::Client::new(),
            &url,
            &cache_path(root.path()),
            false,
            &mut hints,
            1_000,
        )
        .await
        .unwrap();
        // The cached data is recent now, so a non-forced refresh must not
        // issue another request.
        refresh(
            &reqwest::Client::new(),
            &url,
            &cache_path(root.path()),
            false,
            &mut hints,
            2_000,
        )
        .await
        .unwrap();
        let requests = server.await.unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(
            hints.api("opencode-go", "union-alpha"),
            Some("anthropic-messages")
        );
        // The failure stamped the cache and the skipped refresh left it alone.
        assert_eq!(hints.checked_at, Some(1_000));
    }

    #[tokio::test]
    async fn corrupt_cache_files_load_as_empty_hints() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("models-dev.json"), "not json").unwrap();
        assert_eq!(
            load(root.path()).await.api("opencode-go", "union-alpha"),
            None
        );
    }
}
