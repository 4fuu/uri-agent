//! Radius gateway model configuration discovery.

use super::{CatalogCredential, CatalogModel};
use anyhow::{Context, Result, anyhow, bail};
use http::header::{ACCEPT, AUTHORIZATION};
use reqwest::{Client, Url};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) const PROVIDER: &str = "radius";
pub(crate) const DEFAULT_GATEWAY: &str = "https://radius.pi.dev";

pub(crate) async fn discover(
    _client: &Client,
    credential: &CatalogCredential,
) -> Result<Vec<CatalogModel>> {
    let gateway = gateway_url(
        credential
            .radius_gateway
            .as_deref()
            .unwrap_or(DEFAULT_GATEWAY),
    )?;
    discover_at(gateway, credential).await
}

async fn discover_at(
    mut gateway: Url,
    credential: &CatalogCredential,
) -> Result<Vec<CatalogModel>> {
    gateway.set_path("/v1/config");
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(4))
        .build()?;
    let value = client
        .get(gateway.clone())
        .header(ACCEPT, "application/json")
        .header(AUTHORIZATION, format!("Bearer {}", credential.secret))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    parse_config(value, &gateway)
}

fn gateway_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid Radius gateway URL")?;
    let loopback_http = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if (url.scheme() != "https" && !loopback_http)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "Radius gateway URL must be HTTPS or HTTP loopback without credentials, query, or fragment"
        );
    }
    Ok(url)
}

fn parse_config(value: Value, gateway: &Url) -> Result<Vec<CatalogModel>> {
    let base_url = value
        .get("baseUrl")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Radius config has no baseUrl"))?;
    let base_url = Url::parse(base_url).context("invalid Radius config baseUrl")?;
    if base_url.origin() != gateway.origin() {
        bail!("Radius config baseUrl has a different origin than its gateway");
    }
    let records = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Radius config has no models array"))?;
    let mut models = Vec::new();
    for raw in records {
        let Some(id) = raw
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let Some(context) = raw
            .get("contextWindow")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
        else {
            continue;
        };
        let Some(max_tokens) = raw
            .get("maxTokens")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
        else {
            continue;
        };
        let Some(input) = raw.get("input").and_then(Value::as_array) else {
            continue;
        };
        let Some(cost) = raw.get("cost").and_then(Value::as_object) else {
            continue;
        };
        let mut metadata = BTreeMap::from([
            (
                "reasoning".to_string(),
                Value::Bool(
                    raw.get("reasoning")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ),
            ),
            ("input".to_string(), Value::Array(input.clone())),
            ("cost".to_string(), Value::Object(cost.clone())),
            ("contextWindow".to_string(), context.into()),
            ("maxTokens".to_string(), max_tokens.into()),
            ("discovered".to_string(), Value::Bool(true)),
        ]);
        if let Some(map) = raw.get("thinkingLevelMap") {
            metadata.insert("thinkingLevelMap".to_string(), map.clone());
        }
        models.push(CatalogModel {
            id: id.to_string(),
            name: raw
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string(),
            api: "pi-messages".to_string(),
            provider: PROVIDER.to_string(),
            base_url: base_url.to_string(),
            headers: BTreeMap::new(),
            metadata,
        });
    }
    if models.is_empty() {
        bail!("Radius config has no valid models");
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn gateway_config_preserves_published_metadata() {
        let gateway = Url::parse("https://gateway.test/v1/config").unwrap();
        let models = parse_config(
            json!({"baseUrl":"https://gateway.test/v1","models":[{
                "id":"radius-one","name":"Radius One","reasoning":true,"input":["text","image"],
                "cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":0},
                "contextWindow":200000,"maxTokens":32000,"thinkingLevelMap":{"high":"high"}
            }]}),
            &gateway,
        )
        .unwrap();
        assert_eq!(models[0].api, "pi-messages");
        assert_eq!(models[0].context_window(), 200_000);
        assert!(models[0].accepts_input("image"));
    }

    #[test]
    fn gateway_and_config_origins_are_restricted() {
        for invalid in [
            "http://example.com",
            "ftp://example.com",
            "https://u:p@example.com",
            "https://example.com?q=1",
            "https://example.com/#fragment",
        ] {
            assert!(gateway_url(invalid).is_err(), "accepted {invalid}");
        }
        let gateway = Url::parse("https://gateway.test/v1/config").unwrap();
        assert!(
            parse_config(
                json!({"baseUrl":"https://other.test/v1","models":[]}),
                &gateway
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn gateway_fetch_uses_origin_root_auth_and_rejects_cross_origin_config() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let count = socket.read(&mut request).await.unwrap();
            let body = r#"{"baseUrl":"https://attacker.test/v1","models":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request[..count].to_vec()).unwrap()
        });
        let credential = CatalogCredential {
            secret: "radius-secret".into(),
            oauth: false,
            radius_gateway: Some(format!("http://{address}/ignored/path")),
            workbuddy: None,
        };

        assert!(discover(&Client::new(), &credential).await.is_err());
        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/config http/1.1\r\n"));
        assert!(request.contains("authorization: bearer radius-secret\r\n"));
        assert!(request.contains("accept: application/json\r\n"));
    }
}
