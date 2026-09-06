use super::super::device::{self, Poll};
use super::super::util::open_url;
use super::super::{LoginSetup, OauthDisplay, OauthLogin, OauthToken, channels, set_display};
use super::shared::{FormUrlEncoded, json_expires, json_interval, required_str};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Map, Number, Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, watch};

const CLIENT_ID: &str = "1031625952748946";
const DEVICE_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
const TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const KEY_URL: &str = "https://api.meta.ai/muse-code/key";
const API_VERSION: &str = "1.0.0";

pub(in crate::oauth) fn start_muse_code()
-> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let LoginSetup {
        login,
        paste_rx: _,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        "https://auth.meta.com".to_string(),
        Some("starting…".to_string()),
        "Open the Meta verification URL and enter the device code.",
    );
    tokio::spawn(async move {
        let mut outer_cancel = cancel_rx.clone();
        let result = if *outer_cancel.borrow() {
            Err(anyhow!("OAuth login was cancelled"))
        } else {
            tokio::select! {
                result = login_flow(DEVICE_URL, TOKEN_URL, KEY_URL, cancel_rx, display, true) => result,
                _ = outer_cancel.changed() => Err(anyhow!("OAuth login was cancelled")),
            }
        };
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

async fn login_flow(
    device_url: &str,
    token_url: &str,
    key_url: &str,
    cancel_rx: watch::Receiver<bool>,
    display: Arc<Mutex<OauthDisplay>>,
    open_browser: bool,
) -> Result<OauthToken> {
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("uri-agent/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = client
        .post(device_url)
        .header("Accept", "application/json")
        .header("x-api-version", API_VERSION)
        .form_urlencoded(&[("client_id", CLIENT_ID)])
        .send()
        .await
        .context("Muse Code device authorization failed")?;
    let value = safe_json(response, "Muse Code device authorization").await?;
    let device_code = required_str(&value, "device_code")?;
    let user_code = required_str(&value, "user_code")?;
    let verification = value
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| value.get("verification_uri").and_then(Value::as_str))
        .ok_or_else(|| anyhow!("Muse Code device response is missing verification_uri"))?;
    let verification = trusted_meta_verification_url(verification)?;
    set_display(
        &display,
        verification.clone(),
        Some(user_code),
        "Open the Meta verification URL and enter the device code.",
    );
    if open_browser {
        open_url(&verification);
    }
    let account_token = device::poll(
        json_interval(&value),
        json_expires(&value),
        true,
        cancel_rx,
        || {
            let client = client.clone();
            let device_code = device_code.clone();
            async move {
                let response = client
                    .post(token_url)
                    .header("Accept", "application/json")
                    .header("x-api-version", API_VERSION)
                    .form_urlencoded(&[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("client_id", CLIENT_ID),
                        ("device_code", device_code.as_str()),
                    ])
                    .send()
                    .await?;
                token_poll(response).await
            }
        },
    )
    .await?;
    mint_key(&client, key_url, &account_token).await
}

async fn token_poll(response: reqwest::Response) -> Result<Poll<String>> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(Poll::Complete(required_str(&value, "access_token")?));
    }
    Ok(match value.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Poll::Pending,
        Some("slow_down") => Poll::SlowDown {
            interval: json_interval(&value),
        },
        Some("access_denied" | "authorization_denied") => {
            Poll::Failed("Muse Code login was denied".to_string())
        }
        Some("expired_token") => Poll::Failed("Muse Code login expired".to_string()),
        Some(_) => Poll::Failed("Muse Code device token request failed".to_string()),
        None => Poll::Failed(format!("Muse Code device token request failed ({status})")),
    })
}

async fn safe_json(response: reqwest::Response, label: &str) -> Result<Value> {
    let status = response.status();
    if !status.is_success() {
        bail!("{label} failed ({status})");
    }
    response
        .json::<Value>()
        .await
        .with_context(|| format!("{label} returned invalid JSON"))
}

async fn mint_key(client: &Client, key_url: &str, account_token: &str) -> Result<OauthToken> {
    let response = client
        .post(key_url)
        .header("Accept", "application/json")
        .header("x-api-version", API_VERSION)
        .bearer_auth(account_token)
        .json(&json!({"onboard": true}))
        .send()
        .await
        .context("Muse Code key exchange failed")?;
    let value = safe_json(response, "Muse Code key exchange").await?;
    token_from_key_response(account_token, &value)
}

#[derive(Deserialize)]
struct KeyResponse {
    api_key: Option<String>,
    is_subs_active: Option<bool>,
    user_id: Option<String>,
    user_email: Option<String>,
    require_payment: Option<bool>,
    action_url: Option<String>,
    require_payment_action_url: Option<String>,
    subs_tier_id: Option<String>,
    subs_tier_name: Option<String>,
    subs_usage: Option<Value>,
}

fn token_from_key_response(account_token: &str, value: &Value) -> Result<OauthToken> {
    let payload: KeyResponse = serde_json::from_value(value.clone())
        .map_err(|_| anyhow!("Muse Code key response has invalid field types"))?;
    if account_token.trim().is_empty() {
        bail!("Muse Code device response is missing access_token");
    }
    if payload.is_subs_active == Some(false) {
        bail!("Muse Code subscription is inactive; run :login again after subscribing");
    }
    let key = nonempty(payload.api_key).ok_or_else(|| {
        if payload.require_payment == Some(true)
            || nonempty(payload.action_url.clone()).is_some()
            || nonempty(payload.require_payment_action_url.clone()).is_some()
        {
            anyhow!("Muse Code subscription is required; complete payment separately, then run :login again")
        } else {
            anyhow!("Muse Code key response is missing api_key")
        }
    })?;
    let email = nonempty(payload.user_email).map(|value| value.to_lowercase());
    let account_id = nonempty(payload.user_id)
        .or_else(|| email.clone())
        .ok_or_else(|| anyhow!("Muse Code key response is missing a stable account identity"))?;
    let mut extra = BTreeMap::from([
        (
            "oauthAccessToken".to_string(),
            Value::String(account_token.to_string()),
        ),
        ("accountId".to_string(), Value::String(account_id)),
    ]);
    insert_optional(&mut extra, "email", email.map(Value::String));
    insert_optional(
        &mut extra,
        "isSubsActive",
        payload.is_subs_active.map(Value::Bool),
    );
    insert_optional(
        &mut extra,
        "requirePayment",
        payload.require_payment.map(Value::Bool),
    );
    insert_optional(
        &mut extra,
        "actionUrl",
        nonempty(payload.action_url).map(Value::String),
    );
    insert_optional(
        &mut extra,
        "requirePaymentActionUrl",
        nonempty(payload.require_payment_action_url).map(Value::String),
    );
    insert_optional(
        &mut extra,
        "subsTierId",
        nonempty(payload.subs_tier_id).map(Value::String),
    );
    insert_optional(
        &mut extra,
        "subsTierName",
        nonempty(payload.subs_tier_name).map(Value::String),
    );
    if let Some(usage) = payload.subs_usage {
        insert_optional(&mut extra, "subsUsage", validated_usage(&usage)?);
    }
    Ok(OauthToken {
        kind: "oauth".to_string(),
        refresh: String::new(),
        access: key,
        expires: i64::MAX,
        extra,
    })
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn insert_optional(map: &mut BTreeMap<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_string(), value);
    }
}

fn validated_usage(value: &Value) -> Result<Option<Value>> {
    let source = value
        .as_object()
        .ok_or_else(|| anyhow!("Muse Code subs_usage has invalid field types"))?;
    let mut usage = Map::new();
    for name in ["window", "weekly"] {
        if let Some(value) = source.get(name) {
            if value.is_null() {
                continue;
            }
            if let Some(window) = validated_window(value)? {
                usage.insert(name.to_string(), window);
            }
        }
    }
    Ok((!usage.is_empty()).then_some(Value::Object(usage)))
}

fn validated_window(value: &Value) -> Result<Option<Value>> {
    let source = value
        .as_object()
        .ok_or_else(|| anyhow!("Muse Code subscription window has invalid field types"))?;
    let mut window = Map::new();
    for name in ["used_percent", "window_duration_mins"] {
        if source.get(name).is_some_and(|value| !value.is_number()) {
            bail!("Muse Code subscription window has invalid field types");
        }
    }
    if source
        .get("resets_at")
        .is_some_and(|value| !value.is_string() && !value.is_number())
    {
        bail!("Muse Code subscription window has invalid field types");
    }
    if let Some(percent) = source.get("used_percent").and_then(Value::as_f64)
        && percent.is_finite()
        && percent >= 0.0
        && let Some(number) = Number::from_f64(percent.min(100.0))
    {
        window.insert("used_percent".to_string(), Value::Number(number));
    }
    if let Some(reset) = source.get("resets_at") {
        let valid = reset
            .as_i64()
            .filter(|value| *value >= 0)
            .map(Number::from)
            .map(Value::Number)
            .or_else(|| {
                reset
                    .as_str()
                    .filter(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
                    .map(|value| Value::String(value.to_string()))
            });
        if let Some(valid) = valid {
            window.insert("resets_at".to_string(), valid);
        }
    }
    if let Some(duration) = source.get("window_duration_mins").and_then(Value::as_f64)
        && duration.is_finite()
        && duration > 0.0
        && let Some(number) = Number::from_f64(duration)
    {
        window.insert("window_duration_mins".to_string(), Value::Number(number));
    }
    Ok((!window.is_empty()).then_some(Value::Object(window)))
}

fn trusted_meta_verification_url(value: &str) -> Result<String> {
    let url =
        Url::parse(value.trim()).map_err(|_| anyhow!("untrusted Muse Code verification URL"))?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https"
        || !(host == "meta.com" || host.ends_with(".meta.com"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
    {
        bail!("untrusted Muse Code verification URL");
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_response_stores_minted_key_identity_and_valid_usage() {
        let token = token_from_key_response("account-secret", &json!({
            "api_key": "model-secret", "is_subs_active": true,
            "user_id": "user-1", "user_email": "USER@EXAMPLE.COM",
            "subs_tier_id": "pro", "subs_tier_name": "Pro",
            "subs_usage": {"window": {"used_percent": 125, "resets_at": "2026-09-06T12:00:00Z", "window_duration_mins": 60}}
        })).unwrap();
        assert_eq!(token.access, "model-secret");
        assert!(token.refresh.is_empty());
        assert_eq!(token.expires, i64::MAX);
        assert_eq!(token.extra["oauthAccessToken"], "account-secret");
        assert_eq!(token.extra["accountId"], "user-1");
        assert_eq!(token.extra["email"], "user@example.com");
        assert_eq!(token.extra["subsUsage"]["window"]["used_percent"], 100.0);
        let encoded = serde_json::to_string(&token).unwrap();
        assert_eq!(serde_json::from_str::<OauthToken>(&encoded).unwrap(), token);
    }

    #[test]
    fn key_response_rejects_inactive_payment_missing_key_and_identity() {
        for value in [
            json!({"api_key":"key", "user_id":"id", "is_subs_active":false}),
            json!({"require_payment":true, "action_url":"https://pay.example", "user_id":"id"}),
            json!({"api_key":"key"}),
            json!({"api_key":7, "user_id":"id"}),
        ] {
            assert!(token_from_key_response("secret", &value).is_err());
        }
    }

    #[test]
    fn verification_url_must_be_https_on_meta_domain() {
        assert!(trusted_meta_verification_url("https://auth.meta.com/device").is_ok());
        assert!(trusted_meta_verification_url("http://auth.meta.com/device").is_err());
        assert!(trusted_meta_verification_url("https://meta.com.evil.test/device").is_err());
    }

    async fn fixture(
        responses: Vec<(u16, Value)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap_or("0")
                            .parse::<usize>()
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn device_exchange_mints_once_and_uses_separate_credentials() {
        let (url, server) = fixture(vec![
            (200, json!({"device_code":"device","user_code":"ABCD","verification_uri":"https://auth.meta.com/device","interval":1,"expires_in":60})),
            (400, json!({"error":"authorization_pending"})),
            (200, json!({"access_token":"account-secret"})),
            (200, json!({"api_key":"model-secret","user_id":"account"})),
        ]).await;
        let setup = channels(String::new(), None, "test");
        let token = login_flow(
            &format!("{url}/device"),
            &format!("{url}/token"),
            &format!("{url}/key"),
            setup.cancel_rx,
            setup.display,
            false,
        )
        .await
        .unwrap();
        assert_eq!(token.access, "model-secret");
        assert!(!token.expired());
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].contains("client_id=1031625952748946"));
        assert!(!requests[0].contains("scope="));
        assert!(requests[1].contains("device_code=device"));
        assert!(requests[3].contains("Bearer account-secret"));
        assert!(requests[3].contains(r#"{"onboard":true}"#));
        assert!(requests.iter().all(|r| r.contains("x-api-version: 1.0.0")));
    }

    #[tokio::test]
    async fn key_exchange_429_is_not_retried_or_leaked() {
        let (url, server) = fixture(vec![(429, json!({"api_key":"response-secret"}))]).await;
        let error = mint_key(&Client::new(), &url, "account-secret")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("429"));
        assert!(!format!("{error:#}").contains("secret"));
        assert_eq!(server.await.unwrap().len(), 1);
    }
}
