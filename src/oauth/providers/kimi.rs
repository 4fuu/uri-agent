use super::super::device;
use super::super::util::{http_client, open_url, trusted_http_url};
use super::super::{LoginSetup, OauthDisplay, OauthLogin, OauthToken, channels, set_display};
use super::shared::{
    FormUrlEncoded, json_expires, json_interval, json_or_error, oauth_poll_from_token_response,
    read_token_form, required_str,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

fn host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| "https://auth.kimi.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

pub(in crate::oauth) fn start_kimi() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)>
{
    let host = host();
    let LoginSetup {
        login,
        paste_rx: _,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        host.clone(),
        Some("starting…".to_string()),
        "Open Kimi Code and enter the device code.",
    );
    tokio::spawn(async move {
        let result = kimi_login(host, cancel_rx, display).await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

async fn kimi_login(
    host: String,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    display: Arc<Mutex<OauthDisplay>>,
) -> Result<OauthToken> {
    let client = http_client()?;
    let response = client
        .post(format!("{host}/api/oauth/device_authorization"))
        .header("Accept", "application/json")
        .form_urlencoded(&[("client_id", KIMI_CLIENT_ID)])
        .send()
        .await
        .context("Kimi Code device authorization failed")?;
    let value = json_or_error(response, "Kimi Code device authorization").await?;
    let device_code = required_str(&value, "device_code")?;
    let user_code = required_str(&value, "user_code")?;
    let verification = trusted_http_url(
        value
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .or_else(|| value.get("verification_uri").and_then(Value::as_str))
            .unwrap_or_default(),
    )?;
    set_display(
        &display,
        verification.clone(),
        Some(user_code),
        "Open Kimi Code and enter the device code.",
    );
    open_url(&verification);
    device::poll(
        json_interval(&value),
        json_expires(&value).or(Some(Duration::from_secs(15 * 60))),
        true,
        cancel_rx,
        || {
            let client = client.clone();
            let host = host.clone();
            let device_code = device_code.clone();
            async move {
                let response = client
                    .post(format!("{host}/api/oauth/token"))
                    .header("Accept", "application/json")
                    .form_urlencoded(&[
                        ("client_id", KIMI_CLIENT_ID),
                        ("device_code", device_code.as_str()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ])
                    .send()
                    .await?;
                oauth_poll_from_token_response(response, "Kimi Code").await
            }
        },
    )
    .await
}

pub(in crate::oauth) async fn refresh_kimi(refresh: &str) -> Result<OauthToken> {
    let response = http_client()?
        .post(format!("{}/api/oauth/token", host()))
        .form_urlencoded(&[
            ("client_id", KIMI_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .context("Kimi Code token refresh failed")?;
    read_token_form(response, "Kimi Code").await
}
