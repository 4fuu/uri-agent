use super::super::OauthToken;
use super::super::device::Poll;
use super::super::util::form_body;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

pub(super) async fn read_token_json(
    response: reqwest::Response,
    label: &str,
) -> Result<OauthToken> {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{label} token request failed ({status}): {text}");
    }
    let value: Value = serde_json::from_str(&text).context(format!("{label} token is not JSON"))?;
    token_from_value(&value)
}

pub(super) async fn read_token_form(
    response: reqwest::Response,
    label: &str,
) -> Result<OauthToken> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{label} token request failed ({status}): {value}");
    }
    token_from_value(&value)
}

pub(super) async fn json_or_error(response: reqwest::Response, label: &str) -> Result<Value> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{label} failed ({status}): {value}");
    }
    Ok(value)
}

pub(super) async fn oauth_poll_from_token_response(
    response: reqwest::Response,
    label: &str,
) -> Result<Poll<OauthToken>> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(Poll::Complete(token_from_value(&value)?));
    }
    match value.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Ok(Poll::Pending),
        Some("slow_down") => Ok(Poll::SlowDown {
            interval: json_interval(&value),
        }),
        Some("expired_token") => Ok(Poll::Failed(format!(
            "{label} device authorization expired"
        ))),
        Some("access_denied" | "authorization_denied") => {
            Ok(Poll::Failed(format!("{label} login was denied")))
        }
        Some(error) => Ok(Poll::Failed(format!(
            "{label} device token failed: {error}"
        ))),
        None if status.as_u16() >= 500 => Ok(Poll::Failed(format!(
            "{label} device token failed ({status})"
        ))),
        None => Ok(Poll::Pending),
    }
}

fn token_from_value(value: &Value) -> Result<OauthToken> {
    let access = required_str(value, "access_token")?;
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(3600);
    let mut extra = BTreeMap::new();
    if let Some(scope) = value.get("scope").and_then(Value::as_str) {
        extra.insert("scope".to_string(), Value::String(scope.to_string()));
    }
    Ok(OauthToken::from_response(access, refresh, expires_in).with_extra(extra))
}

pub(super) fn required_str(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing {field}"))
}

pub(super) fn json_interval(value: &Value) -> Option<Duration> {
    value
        .get("interval")
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|value| *value > 0.0)
        .map(Duration::from_secs_f64)
}

pub(super) fn json_expires(value: &Value) -> Option<Duration> {
    value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
}

pub(super) trait FormUrlEncoded {
    fn form_urlencoded(self, fields: &[(&str, &str)]) -> Self;
}

impl FormUrlEncoded for reqwest::RequestBuilder {
    fn form_urlencoded(self, fields: &[(&str, &str)]) -> Self {
        self.header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body(fields))
    }
}

pub(super) fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::getrandom(&mut buffer)
        .map_err(|error| anyhow!("cannot generate OAuth state: {error}"))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}
