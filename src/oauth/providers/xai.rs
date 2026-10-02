use super::super::util::{http_client, trusted_http_url};
use super::super::{OauthLogin, OauthToken};
use super::shared::{
    FormUrlEncoded, LoginFlow, device_flow, read_token_form, required_str, spawn_login_flow,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::sync::oneshot;

const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

pub(in crate::oauth) fn start_xai() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    spawn_login_flow(
        "https://auth.x.ai".to_string(),
        Some("starting…".to_string()),
        "Sign in with SuperGrok or X Premium using the device code.",
        xai_login,
    )
}

async fn xai_login(flow: LoginFlow) -> Result<OauthToken> {
    let client = http_client()?;
    let token_client = client.clone();
    device_flow(
        flow.cancel_rx,
        flow.display,
        "Sign in with SuperGrok or X Premium using the device code.",
        "xAI",
        client
            .post("https://auth.x.ai/oauth2/device/code")
            .form_urlencoded(&[
                ("client_id", XAI_CLIENT_ID),
                (
                    "scope",
                    "openid profile email offline_access grok-cli:access api:access",
                ),
                ("referrer", "pi"),
            ]),
        |value| {
            let fallback = required_str(value, "verification_uri")?;
            let verification = value
                .get("verification_uri_complete")
                .and_then(Value::as_str)
                .unwrap_or(fallback.as_str());
            let verification = trusted_http_url(verification)?;
            if !verification.starts_with("https://") {
                bail!("Untrusted verification URI in xAI OAuth response");
            }
            Ok(verification)
        },
        true,
        None,
        move |device_code| {
            token_client
                .post("https://auth.x.ai/oauth2/token")
                .form_urlencoded(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", XAI_CLIENT_ID),
                    ("device_code", device_code),
                ])
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
