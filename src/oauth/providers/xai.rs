use super::super::device;
use super::super::util::{http_client, open_url, trusted_http_url};
use super::super::{LoginSetup, OauthDisplay, OauthLogin, OauthToken, channels, set_display};
use super::shared::{
    FormUrlEncoded, json_expires, json_interval, json_or_error, oauth_poll_from_token_response,
    read_token_form, required_str,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

pub(in crate::oauth) fn start_xai() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let LoginSetup {
        login,
        paste_rx: _,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        "https://auth.x.ai".to_string(),
        Some("starting…".to_string()),
        "Sign in with SuperGrok or X Premium using the device code.",
    );
    tokio::spawn(async move {
        let result = xai_login(cancel_rx, display).await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

async fn xai_login(
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    display: Arc<Mutex<OauthDisplay>>,
) -> Result<OauthToken> {
    let client = http_client()?;
    let response = client
        .post("https://auth.x.ai/oauth2/device/code")
        .form_urlencoded(&[
            ("client_id", XAI_CLIENT_ID),
            (
                "scope",
                "openid profile email offline_access grok-cli:access api:access",
            ),
            ("referrer", "pi"),
        ])
        .send()
        .await
        .context("xAI device authorization failed")?;
    let value = json_or_error(response, "xAI device authorization").await?;
    let device_code = required_str(&value, "device_code")?;
    let verification = trusted_http_url(
        value
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .unwrap_or(&required_str(&value, "verification_uri")?),
    )?;
    if !verification.starts_with("https://") {
        bail!("Untrusted verification URI in xAI OAuth response");
    }
    set_display(
        &display,
        verification.clone(),
        Some(required_str(&value, "user_code")?),
        "Sign in with SuperGrok or X Premium using the device code.",
    );
    open_url(&verification);
    device::poll(
        json_interval(&value),
        json_expires(&value),
        true,
        cancel_rx,
        || {
            let client = client.clone();
            let device_code = device_code.clone();
            async move {
                let response = client
                    .post("https://auth.x.ai/oauth2/token")
                    .form_urlencoded(&[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("client_id", XAI_CLIENT_ID),
                        ("device_code", device_code.as_str()),
                    ])
                    .send()
                    .await?;
                oauth_poll_from_token_response(response, "xAI").await
            }
        },
    )
    .await
}

pub(in crate::oauth) async fn refresh_xai(refresh: &str) -> Result<OauthToken> {
    let response = http_client()?
        .post("https://auth.x.ai/oauth2/token")
        .form_urlencoded(&[
            ("grant_type", "refresh_token"),
            ("client_id", XAI_CLIENT_ID),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .context("xAI token refresh failed")?;
    let mut token = read_token_form(response, "xAI").await?;
    if token.refresh.is_empty() {
        token.refresh = refresh.to_string();
    }
    Ok(token)
}
