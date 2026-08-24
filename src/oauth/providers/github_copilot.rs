use super::super::device::{self, Poll};
use super::super::util::{decode_b64, extra_string, http_client, open_url, trusted_http_url};
use super::super::{LoginSetup, OauthDisplay, OauthLogin, OauthToken, channels, set_display};
use super::shared::{FormUrlEncoded, json_expires, json_interval, json_or_error, required_str};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

const COPILOT_CLIENT_ID_B64: &str = "SXYxLmI1MDdhMDhjODdlY2ZlOTg=";

pub(in crate::oauth) fn start_github_copilot(
    domain: Option<&String>,
) -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let domain = normalize_github_domain(domain.cloned().unwrap_or_default())?;
    let enterprise = (domain != "github.com").then(|| domain.clone());
    let LoginSetup {
        login,
        paste_rx: _,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        format!("https://{domain}/login/device"),
        Some("starting…".to_string()),
        "Open GitHub, enter the device code, then return here.",
    );
    tokio::spawn(async move {
        let result = github_copilot_login(domain, enterprise, cancel_rx, display).await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

async fn github_copilot_login(
    domain: String,
    enterprise: Option<String>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    display: Arc<Mutex<OauthDisplay>>,
) -> Result<OauthToken> {
    let client_id = decode_b64(COPILOT_CLIENT_ID_B64)?;
    let client = http_client()?;
    let response = client
        .post(format!("https://{domain}/login/device/code"))
        .header("Accept", "application/json")
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .form_urlencoded(&[("client_id", client_id.as_str()), ("scope", "read:user")])
        .send()
        .await
        .context("GitHub device code request failed")?;
    let value = json_or_error(response, "GitHub Copilot device code").await?;
    let device_code = required_str(&value, "device_code")?;
    let user_code = required_str(&value, "user_code")?;
    let verification = trusted_http_url(&required_str(&value, "verification_uri")?)?;
    set_display(
        &display,
        verification.clone(),
        Some(user_code.clone()),
        "Open GitHub, enter the device code, then return here.",
    );
    open_url(&verification);
    let github_token = device::poll(
        json_interval(&value),
        json_expires(&value),
        true,
        cancel_rx,
        || {
            let client = client.clone();
            let client_id = client_id.clone();
            let device_code = device_code.clone();
            let domain = domain.clone();
            async move {
                let response = client
                    .post(format!("https://{domain}/login/oauth/access_token"))
                    .header("Accept", "application/json")
                    .header("User-Agent", "GitHubCopilotChat/0.35.0")
                    .form_urlencoded(&[
                        ("client_id", client_id.as_str()),
                        ("device_code", device_code.as_str()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ])
                    .send()
                    .await?;
                let value = response.json::<Value>().await.unwrap_or(Value::Null);
                if let Some(token) = value.get("access_token").and_then(Value::as_str) {
                    return Ok(Poll::Complete(token.to_string()));
                }
                match value.get("error").and_then(Value::as_str) {
                    Some("authorization_pending") => Ok(Poll::Pending),
                    Some("slow_down") => Ok(Poll::SlowDown {
                        interval: json_interval(&value),
                    }),
                    Some(error) => Ok(Poll::Failed(format!("Device flow failed: {error}"))),
                    None => Ok(Poll::Failed("Invalid GitHub device token response".into())),
                }
            }
        },
    )
    .await?;
    refresh_github_copilot_access(&github_token, enterprise.as_deref()).await
}

pub(in crate::oauth) async fn refresh_github_copilot(token: &OauthToken) -> Result<OauthToken> {
    let enterprise = extra_string(&token.extra, "enterpriseUrl");
    refresh_github_copilot_access(&token.refresh, enterprise.as_deref()).await
}
async fn refresh_github_copilot_access(
    github_token: &str,
    enterprise: Option<&str>,
) -> Result<OauthToken> {
    let domain = enterprise.unwrap_or("github.com");
    let response = http_client()?
        .get(format!("https://api.{domain}/copilot_internal/v2/token"))
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {github_token}"))
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .header("Editor-Version", "vscode/1.107.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .send()
        .await
        .context("GitHub Copilot token request failed")?;
    let value = json_or_error(response, "GitHub Copilot token").await?;
    let access = required_str(&value, "token")?;
    let expires_at = value
        .get("expires_at")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("Invalid Copilot token response fields"))?;
    let mut extra = BTreeMap::new();
    extra.insert(
        "refresh".to_string(),
        Value::String(github_token.to_string()),
    );
    if let Some(enterprise) = enterprise {
        extra.insert(
            "enterpriseUrl".to_string(),
            Value::String(enterprise.to_string()),
        );
    }
    Ok(OauthToken {
        kind: "oauth".to_string(),
        refresh: github_token.to_string(),
        access,
        expires: expires_at * 1000 - 5 * 60 * 1000,
        extra,
    })
}
fn normalize_github_domain(input: String) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok("github.com".to_string());
    }
    let uri = if trimmed.contains("://") {
        http::Uri::try_from(trimmed)
    } else {
        http::Uri::try_from(format!("https://{trimmed}"))
    }
    .map_err(|_| anyhow!("Invalid GitHub Enterprise URL/domain"))?;
    uri.host()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Invalid GitHub Enterprise URL/domain"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn client_id_matches_pi() {
        assert_eq!(
            decode_b64(COPILOT_CLIENT_ID_B64).unwrap(),
            "Iv1.b507a08c87ecfe98"
        );
    }
    #[test]
    fn domain_defaults_and_strips_scheme() {
        assert_eq!(
            normalize_github_domain(String::new()).unwrap(),
            "github.com"
        );
        assert_eq!(
            normalize_github_domain("https://company.ghe.com".into()).unwrap(),
            "company.ghe.com"
        );
    }
}
