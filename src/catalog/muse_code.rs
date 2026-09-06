//! Muse Code's credential-scoped, authoritative model roster.

use super::{CatalogCredential, CatalogModel};
use anyhow::{Result, bail};
use http::header::{ACCEPT, AUTHORIZATION};
use reqwest::{Client, Url};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) const PROVIDER: &str = "muse-code";
const BASE_URL: &str = "https://api.meta.ai/v1";

pub(crate) async fn discover(
    _client: &Client,
    credential: &CatalogCredential,
) -> Result<Vec<CatalogModel>> {
    let endpoint = Url::parse("https://api.meta.ai/v1/models").expect("fixed Muse URL is valid");
    discover_at(endpoint, credential).await
}

async fn discover_at(endpoint: Url, credential: &CatalogCredential) -> Result<Vec<CatalogModel>> {
    // Do not allow a response to forward the subscription-minted key away
    // from Meta's fixed endpoint.
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(4))
        .build()?;
    let value = client
        .get(endpoint)
        .header(ACCEPT, "application/json")
        .header(AUTHORIZATION, format!("Bearer {}", credential.secret))
        .header("x-api-version", "1.0.0")
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    parse_roster(value)
}

fn parse_roster(value: Value) -> Result<Vec<CatalogModel>> {
    let records = value.get("data").and_then(Value::as_array);
    let Some(records) = records else {
        bail!("Muse model listing has no data array");
    };
    let models = records
        .iter()
        .filter_map(|record| record.get("id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .filter(|id| !id.starts_with("muse-image-") && !id.starts_with("muse-voice-"))
        .map(|id| (id.to_string(), model(id, true)))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect::<Vec<_>>();
    Ok(models)
}

pub(crate) fn seeds() -> BTreeMap<String, Value> {
    [
        "muse-spark-1.1",
        "muse-spark-1.2",
        "muse-spark-1.2-contributor",
        "muse-spark-1.3",
        "muse-spark-1.3-contributor",
    ]
    .into_iter()
    .map(|id| {
        (
            id.to_string(),
            serde_json::to_value(model(id, false)).unwrap(),
        )
    })
    .collect()
}

fn model(id: &str, discovered: bool) -> CatalogModel {
    let contributor = id.ends_with("-contributor");
    let known = matches!(
        id,
        "muse-spark-1.1"
            | "muse-spark-1.2"
            | "muse-spark-1.2-contributor"
            | "muse-spark-1.3"
            | "muse-spark-1.3-contributor"
    );
    let mut metadata = BTreeMap::from([
        ("reasoning".to_string(), Value::Bool(true)),
        ("input".to_string(), json!(["text", "image"])),
        ("contextWindow".to_string(), Value::from(1_048_576)),
        ("maxTokens".to_string(), Value::from(131_072)),
        (
            "thinkingLevelMap".to_string(),
            json!({
                "off": null, "minimal": "minimal", "low": "low", "medium": "medium",
                "high": "high", "xhigh": "xhigh",
                "max": if id == "muse-spark-1.3" { json!("max") } else { Value::Null }
            }),
        ),
        (
            "compat".to_string(),
            json!({"supportsReasoningEffort": true, "includeEncryptedReasoning": true}),
        ),
    ]);
    // Live responses only identify models. Do not claim that an unknown SKU
    // has a verified price, even when its capabilities match the Muse lineage.
    if known {
        metadata.insert(
            "cost".to_string(),
            if contributor {
                json!({"input": 0.1, "output": 0.2, "cacheRead": 0.002, "cacheWrite": 0})
            } else {
                json!({"input": 1.25, "output": 4.25, "cacheRead": 0.15, "cacheWrite": 0})
            },
        );
    }
    if discovered {
        metadata.insert("discovered".to_string(), Value::Bool(true));
    }
    CatalogModel {
        id: id.to_string(),
        name: id.to_string(),
        api: "openai-responses".to_string(),
        provider: PROVIDER.to_string(),
        base_url: BASE_URL.to_string(),
        headers: BTreeMap::from([("x-api-version".to_string(), "1.0.0".to_string())]),
        metadata,
    }
}

pub(super) fn enforce_transport(model: &mut Value) {
    let Some(object) = model.as_object_mut() else {
        return;
    };
    object.insert("baseUrl".to_string(), Value::String(BASE_URL.to_string()));
    object.insert("headers".to_string(), json!({"x-api-version": "1.0.0"}));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn roster_filters_media_and_prices_only_known_skus() {
        let models = parse_roster(json!({"data": [
            {"id": "muse-spark-1.3"}, {"id": "muse-spark-2.0"},
            {"id": "muse-image-1"}, {"id": "muse-voice-1"}
        ]}))
        .unwrap();
        assert_eq!(models.len(), 2);
        assert!(models[0].metadata.contains_key("cost"));
        assert!(!models[1].metadata.contains_key("cost"));
        assert!(models[0].supports_thinking_level(super::super::ThinkingLevel::Max));
        assert!(!models[1].supports_thinking_level(super::super::ThinkingLevel::Max));
    }

    #[test]
    fn empty_and_media_only_rosters_are_authoritative() {
        assert!(parse_roster(json!({"data": []})).unwrap().is_empty());
        assert!(
            parse_roster(json!({"data": [{"id": "muse-image-1"}, {"id": "muse-voice-1"}]}))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn request_uses_fixed_headers_and_does_not_follow_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let count = socket.read(&mut request).await.unwrap();
            let response = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request[..count].to_vec()).unwrap()
        });
        let credential = CatalogCredential {
            secret: "muse-secret".into(),
            oauth: true,
            radius_gateway: None,
            workbuddy: None,
        };

        assert!(
            discover_at(
                Url::parse(&format!("http://{address}/models")).unwrap(),
                &credential
            )
            .await
            .is_err()
        );
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /models http/1.1\r\n"));
        assert!(request.contains("authorization: bearer muse-secret\r\n"));
        assert!(request.contains("accept: application/json\r\n"));
        assert!(request.contains("x-api-version: 1.0.0\r\n"));
    }
}
