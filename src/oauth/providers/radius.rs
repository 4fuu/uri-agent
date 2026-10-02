use super::super::callback;
use super::super::util::{
    encode, extra_string, generate_pkce, http_client, open_url, trusted_http_url,
};
use super::super::{OauthLogin, OauthToken, set_display};
use super::shared::{
    FormUrlEncoded, LoginFlow, device_flow, json_or_error, read_token_form, required_str,
    spawn_login_flow,
};
use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::oneshot;

const RADIUS_CLIENT_ID: &str = "pi-gateway";
const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

pub(in crate::oauth) fn start_radius_browser(
    gateway: Option<&str>,
) -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let gateway = normalize_gateway(gateway);
    spawn_login_flow(
        gateway.clone(),
        None,
        "Complete Radius login in the browser, or paste the redirect URL / code.",
        move |mut flow| async move {
            let discovery = http_client()?
                .get(format!("{gateway}/v1/oauth"))
                .header("Accept", "application/json")
                .send()
                .await
                .context("Could not load Radius OAuth config")?;
            let config = json_or_error(discovery, "Radius OAuth config").await?;
            let authorize = required_str(&config, "authorizationEndpoint")?;
            let pkce = generate_pkce()?;
            let state = uuid::Uuid::now_v7().simple().to_string();
            let url = format!(
                "{authorize}{}response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&handoff=url&state={}",
                if authorize.contains('?') { "&" } else { "?" },
                encode(RADIUS_CLIENT_ID),
                encode("http://127.0.0.1:1456/oauth/callback"),
                encode("gateway offline_access"),
                encode(&pkce.challenge),
                encode(&state)
            );
            set_display(
                &flow.display,
                url.clone(),
                None,
                "Complete Radius login in the browser, or paste the redirect URL / code.",
            );
            open_url(&url);
            let callback = callback::bind(
                "127.0.0.1",
                1456,
                "/oauth/callback",
                "http://127.0.0.1:1456/oauth/callback",
                Some(&state),
                "Signed in to Radius. You may now close this page.",
            )
            .await;
            let (code, _) = callback::race_callback_or_paste(
                &callback,
                &mut flow.paste_rx,
                &mut flow.cancel_rx,
                Some(&state),
            )
            .await?;
            radius_token(
                &gateway,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", RADIUS_CLIENT_ID),
                    ("redirect_uri", "http://127.0.0.1:1456/oauth/callback"),
                    ("code", &code),
                    ("code_verifier", &pkce.verifier),
                ],
            )
            .await
        },
    )
}

pub(in crate::oauth) fn start_radius_device(
    gateway: Option<&str>,
) -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let gateway = normalize_gateway(gateway);
    spawn_login_flow(
        gateway.clone(),
        Some("starting…".to_string()),
        "Open the Radius verification URL and enter the device code.",
        move |flow| radius_device_login(gateway, flow),
    )
}

async fn radius_device_login(gateway: String, flow: LoginFlow) -> Result<OauthToken> {
    let client = http_client()?;
    let token_client = client.clone();
    let token_url = format!("{gateway}/v1/oauth/token");
    let mut token = device_flow(
        flow.cancel_rx,
        flow.display,
        "Open the Radius verification URL and enter the device code.",
        "Radius",
        client
            .post(format!("{gateway}/v1/oauth/device"))
            .form_urlencoded(&[
                ("client_id", RADIUS_CLIENT_ID),
                ("scope", "gateway offline_access"),
            ]),
        |value| trusted_http_url(&required_str(value, "verification_uri")?),
        false,
        None,
        move |device_code| {
            token_client.post(token_url.clone()).form_urlencoded(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", RADIUS_CLIENT_ID),
                ("device_code", device_code),
            ])
        },
    )
    .await?;
    token
        .extra
        .insert("gateway".to_string(), Value::String(gateway));
    Ok(token)
}

pub(in crate::oauth) async fn refresh_radius(token: &OauthToken) -> Result<OauthToken> {
    let gateway =
        extra_string(&token.extra, "gateway").unwrap_or_else(|| DEFAULT_RADIUS_GATEWAY.to_string());
    radius_token(
        &gateway,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", RADIUS_CLIENT_ID),
            ("refresh_token", &token.refresh),
        ],
    )
    .await
}
async fn radius_token(gateway: &str, body: &[(&str, &str)]) -> Result<OauthToken> {
    let response = http_client()?
        .post(format!("{gateway}/v1/oauth/token"))
        .form_urlencoded(body)
        .send()
        .await
        .context("Radius OAuth token request failed")?;
    let mut token = read_token_form(response, "Radius").await?;
    token.extra.insert(
        "gateway".to_string(),
        Value::String(gateway.trim_end_matches('/').to_string()),
    );
    Ok(token)
}
fn normalize_gateway(gateway: Option<&str>) -> String {
    let value = gateway
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_RADIUS_GATEWAY);
    let with_scheme = if value.contains("://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_string()
}
