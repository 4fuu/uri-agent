use super::super::callback;
use super::super::device::{self, Poll};
use super::super::util::{encode, generate_pkce, http_client, open_url};
use super::super::{LoginSetup, OauthLogin, OauthToken, channels, set_display};
use super::shared::{FormUrlEncoded, json_interval, random_hex, read_token_form, required_str};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::oneshot;

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub(in crate::oauth) fn start_codex_browser()
-> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let pkce = generate_pkce()?;
    let state = random_hex(16)?;
    let url = format!(
        "https://auth.openai.com/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=pi",
        encode(CODEX_CLIENT_ID),
        encode("http://localhost:1455/auth/callback"),
        encode("openid profile email offline_access"),
        encode(&pkce.challenge),
        encode(&state)
    );
    let LoginSetup {
        login,
        mut paste_rx,
        mut cancel_rx,
        done_tx,
        done_rx,
        display: _,
    } = channels(
        url.clone(),
        None,
        "Complete ChatGPT login in the browser, or paste the redirect URL / code.",
    );
    open_url(&url);
    tokio::spawn(async move {
        let result = async {
            let callback = callback::bind(
                "127.0.0.1",
                1455,
                "/auth/callback",
                "http://localhost:1455/auth/callback",
                Some(&state),
                "OpenAI authentication completed. You can close this window.",
            )
            .await;
            let (code, _) = callback::race_callback_or_paste(
                &callback,
                &mut paste_rx,
                &mut cancel_rx,
                Some(&state),
            )
            .await?;
            codex_exchange(&code, &pkce.verifier, "http://localhost:1455/auth/callback").await
        }
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

pub(in crate::oauth) fn start_codex_device()
-> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let LoginSetup {
        login,
        paste_rx: _,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        "https://auth.openai.com/codex/device".to_string(),
        Some("starting…".to_string()),
        "Open the verification URL and enter the device code.",
    );
    tokio::spawn(async move {
        let result = async {
            let client = http_client()?;
            let response = client
                .post("https://auth.openai.com/api/accounts/deviceauth/usercode")
                .json(&json!({ "client_id": CODEX_CLIENT_ID }))
                .send()
                .await
                .context("OpenAI Codex device code request failed")?;
            let status = response.status();
            let value = response.json::<Value>().await.unwrap_or(Value::Null);
            if !status.is_success() {
                bail!("OpenAI Codex device code request failed ({status}): {value}");
            }
            let device_auth_id = required_str(&value, "device_auth_id")?;
            let user_code = required_str(&value, "user_code")?;
            let interval = json_interval(&value);
            set_display(
                &display,
                "https://auth.openai.com/codex/device",
                Some(user_code.clone()),
                "Open the verification URL and enter the device code.",
            );
            open_url("https://auth.openai.com/codex/device");
            let payload = device::poll(
                interval,
                Some(Duration::from_secs(15 * 60)),
                false,
                cancel_rx,
                || {
                    let client = client.clone();
                    let device_auth_id = device_auth_id.clone();
                    let user_code = user_code.clone();
                    async move {
                        let response = client
                            .post("https://auth.openai.com/api/accounts/deviceauth/token")
                            .json(
                                &json!({"device_auth_id": device_auth_id, "user_code": user_code}),
                            )
                            .send()
                            .await?;
                        if response.status().is_success() {
                            let value = response.json::<Value>().await.unwrap_or(Value::Null);
                            return Ok(Poll::Complete((
                                required_str(&value, "authorization_code")?,
                                required_str(&value, "code_verifier")?,
                            )));
                        }
                        if matches!(response.status().as_u16(), 403 | 404) {
                            return Ok(Poll::Pending);
                        }
                        let text = response.text().await.unwrap_or_default();
                        if text.contains("deviceauth_authorization_pending") {
                            return Ok(Poll::Pending);
                        }
                        if text.contains("slow_down") {
                            return Ok(Poll::SlowDown { interval: None });
                        }
                        Ok(Poll::Failed(format!(
                            "OpenAI Codex device auth failed: {text}"
                        )))
                    }
                },
            )
            .await?;
            codex_exchange(
                &payload.0,
                &payload.1,
                "https://auth.openai.com/deviceauth/callback",
            )
            .await
        }
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

pub(in crate::oauth) async fn refresh_codex(refresh: &str) -> Result<OauthToken> {
    let response = http_client()?
        .post("https://auth.openai.com/oauth/token")
        .form_urlencoded(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CODEX_CLIENT_ID),
        ])
        .send()
        .await
        .context("OpenAI Codex token refresh failed")?;
    credentials_from_codex(read_token_form(response, "OpenAI Codex").await?)
}
async fn codex_exchange(code: &str, verifier: &str, redirect: &str) -> Result<OauthToken> {
    let response = http_client()?
        .post("https://auth.openai.com/oauth/token")
        .form_urlencoded(&[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
        ])
        .send()
        .await
        .context("OpenAI Codex token exchange failed")?;
    credentials_from_codex(read_token_form(response, "OpenAI Codex").await?)
}
fn credentials_from_codex(token: OauthToken) -> Result<OauthToken> {
    let account_id = chatgpt_account_id(&token.access)?;
    Ok(token.with_extra(BTreeMap::from([(
        "accountId".to_string(),
        Value::String(account_id),
    )])))
}

pub(crate) fn chatgpt_account_id(access: &str) -> Result<String> {
    let payload = access
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("OpenAI Codex token is not a JWT"))?;
    let mut padded = payload.replace('-', "+").replace('_', "/");
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, padded)
        .context("cannot decode OpenAI Codex token")?;
    let value: Value = serde_json::from_slice(&bytes).context("cannot parse OpenAI Codex token")?;
    value
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .or_else(|| {
            value
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
        })
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Failed to extract accountId from token"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    #[test]
    fn extracts_chatgpt_account_id_from_access_token() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(
                &json!({"https://api.openai.com/auth":{"chatgpt_account_id":"account-123"}}),
            )
            .unwrap(),
        );
        assert_eq!(
            chatgpt_account_id(&format!("header.{payload}.signature")).unwrap(),
            "account-123"
        );
    }
}
