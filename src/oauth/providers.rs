use super::callback;
use super::device::{self, Poll};
use super::util::{
    decode_b64, encode, extra_string, form_body, generate_pkce, http_client, open_url,
    trusted_http_url,
};
use super::{LoginSetup, OauthLogin, OauthToken, channels, set_display};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::oneshot;

const ANTHROPIC_CLIENT_ID_B64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
const COPILOT_CLIENT_ID_B64: &str = "SXYxLmI1MDdhMDhjODdlY2ZlOTg=";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const RADIUS_CLIENT_ID: &str = "pi-gateway";
const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

pub(super) fn start_anthropic() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let pkce = generate_pkce()?;
    let client_id = decode_b64(ANTHROPIC_CLIENT_ID_B64)?;
    let url = format!(
        "https://claude.ai/oauth/authorize?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        encode(&client_id),
        encode("http://localhost:53692/callback"),
        encode(
            "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
        ),
        encode(&pkce.challenge),
        encode(&pkce.verifier)
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
        "Complete login in the browser, or paste the redirect URL / code.",
    );
    open_url(&url);
    tokio::spawn(async move {
        let result = async {
            let callback = callback::bind(
                "127.0.0.1",
                53692,
                "/callback",
                "http://localhost:53692/callback",
                Some(&pkce.verifier),
                "Anthropic authentication completed. You can close this window.",
            )
            .await;
            let (code, state) = callback::race_callback_or_paste(
                &callback,
                &mut paste_rx,
                &mut cancel_rx,
                Some(&pkce.verifier),
            )
            .await?;
            anthropic_exchange(
                &code,
                state.as_deref().unwrap_or(&pkce.verifier),
                &pkce.verifier,
            )
            .await
        }
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

pub(super) async fn refresh_anthropic(refresh: &str) -> Result<OauthToken> {
    anthropic_token(&[
        ("grant_type", "refresh_token"),
        ("client_id", &decode_b64(ANTHROPIC_CLIENT_ID_B64)?),
        ("refresh_token", refresh),
    ])
    .await
}

async fn anthropic_exchange(code: &str, state: &str, verifier: &str) -> Result<OauthToken> {
    anthropic_token(&[
        ("grant_type", "authorization_code"),
        ("client_id", &decode_b64(ANTHROPIC_CLIENT_ID_B64)?),
        ("code", code),
        ("state", state),
        ("redirect_uri", "http://localhost:53692/callback"),
        ("code_verifier", verifier),
    ])
    .await
}

async fn anthropic_token(body: &[(&str, &str)]) -> Result<OauthToken> {
    let response = http_client()?
        .post("https://platform.claude.com/v1/oauth/token")
        .header("Accept", "application/json")
        .json(&body.iter().copied().collect::<BTreeMap<_, _>>())
        .send()
        .await
        .context("Anthropic token request failed")?;
    read_token_json(response, "Anthropic").await
}

pub(super) fn start_openrouter() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let LoginSetup {
        login: login_holder,
        mut paste_rx,
        mut cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        String::new(),
        None,
        "Complete sign-in in the browser, or paste the redirect URL / code.",
    );
    tokio::spawn(async move {
        let result = async {
            let pkce = generate_pkce()?;
            let path = format!("/oauth/callback/{}", uuid::Uuid::now_v7().simple());
            let callback = callback::bind_ephemeral(
                "127.0.0.1",
                &path,
                None,
                "Signed in to OpenRouter. You may now close this page.",
            )
            .await?;
            let authorize = format!(
                "https://openrouter.ai/auth?callback_url={}&code_challenge={}&code_challenge_method=S256",
                encode(&callback.redirect_uri),
                encode(&pkce.challenge)
            );
            set_display(
                &display,
                authorize.clone(),
                None,
                "Complete sign-in in the browser, or paste the redirect URL / code.",
            );
            open_url(&authorize);
            let (code, _) =
                callback::race_callback_or_paste(&callback, &mut paste_rx, &mut cancel_rx, None)
                    .await?;
            openrouter_exchange(&code, &pkce.verifier).await
        }
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login_holder, done_rx))
}

async fn openrouter_exchange(code: &str, verifier: &str) -> Result<OauthToken> {
    let response = http_client()?
        .post("https://openrouter.ai/api/v1/auth/keys")
        .header("Accept", "application/json")
        .json(&json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256"
        }))
        .send()
        .await
        .context("OpenRouter token request failed")?;
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("OpenRouter OAuth key exchange failed ({status}): {value}");
    }
    let key = value
        .get("key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("OpenRouter OAuth response carries no key"))?;
    Ok(OauthToken {
        kind: "oauth".to_string(),
        refresh: String::new(),
        access: key.to_string(),
        expires: i64::MAX / 4,
        extra: BTreeMap::new(),
    })
}

pub(super) fn start_codex_browser() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
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

pub(super) fn start_codex_device() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
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
                            .json(&json!({
                                "device_auth_id": device_auth_id,
                                "user_code": user_code
                            }))
                            .send()
                            .await?;
                        if response.status().is_success() {
                            let value = response.json::<Value>().await.unwrap_or(Value::Null);
                            let code = required_str(&value, "authorization_code")?;
                            let verifier = required_str(&value, "code_verifier")?;
                            return Ok(Poll::Complete((code, verifier)));
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

pub(super) async fn refresh_codex(refresh: &str) -> Result<OauthToken> {
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
    let token = read_token_form(response, "OpenAI Codex").await?;
    credentials_from_codex(token)
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
    let token = read_token_form(response, "OpenAI Codex").await?;
    credentials_from_codex(token)
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

pub(super) fn start_github_copilot(
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
    display: std::sync::Arc<std::sync::Mutex<super::OauthDisplay>>,
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
    let interval = json_interval(&value);
    let expires = json_expires(&value);
    set_display(
        &display,
        verification.clone(),
        Some(user_code.clone()),
        "Open GitHub, enter the device code, then return here.",
    );
    open_url(&verification);
    let github_token = device::poll(interval, expires, true, cancel_rx, || {
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
    })
    .await?;
    refresh_github_copilot_access(&github_token, enterprise.as_deref()).await
}

pub(super) async fn refresh_github_copilot(token: &OauthToken) -> Result<OauthToken> {
    let enterprise = extra_string(&token.extra, "enterpriseUrl");
    refresh_github_copilot_access(&token.refresh, enterprise.as_deref()).await
}

async fn refresh_github_copilot_access(
    github_token: &str,
    enterprise: Option<&str>,
) -> Result<OauthToken> {
    let domain = enterprise.unwrap_or("github.com");
    let url = format!("https://api.{domain}/copilot_internal/v2/token");
    let response = http_client()?
        .get(url)
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

pub(super) fn start_kimi() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let host = std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| "https://auth.kimi.com".to_string())
        .trim_end_matches('/')
        .to_string();
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
    display: std::sync::Arc<std::sync::Mutex<super::OauthDisplay>>,
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

pub(super) async fn refresh_kimi(refresh: &str) -> Result<OauthToken> {
    let host = std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| "https://auth.kimi.com".to_string())
        .trim_end_matches('/')
        .to_string();
    let response = http_client()?
        .post(format!("{host}/api/oauth/token"))
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

pub(super) fn start_xai() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
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
    display: std::sync::Arc<std::sync::Mutex<super::OauthDisplay>>,
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

pub(super) async fn refresh_xai(refresh: &str) -> Result<OauthToken> {
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

pub(super) fn start_radius_browser(
    gateway: Option<&str>,
) -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let gateway = normalize_gateway(gateway);
    let LoginSetup {
        login,
        mut paste_rx,
        mut cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        gateway.clone(),
        None,
        "Complete Radius login in the browser, or paste the redirect URL / code.",
    );
    tokio::spawn(async move {
        let result = async {
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
                &display,
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
            let (code, _) =
                callback::race_callback_or_paste(&callback, &mut paste_rx, &mut cancel_rx, Some(&state))
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
        }
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

pub(super) fn start_radius_device(
    gateway: Option<&str>,
) -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let gateway = normalize_gateway(gateway);
    let LoginSetup {
        login,
        paste_rx: _,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        gateway.clone(),
        Some("starting…".to_string()),
        "Open the Radius verification URL and enter the device code.",
    );
    tokio::spawn(async move {
        let result = async {
            let client = http_client()?;
            let response = client
                .post(format!("{gateway}/v1/oauth/device"))
                .form_urlencoded(&[
                    ("client_id", RADIUS_CLIENT_ID),
                    ("scope", "gateway offline_access"),
                ])
                .send()
                .await
                .context("Radius device authorization failed")?;
            let value = json_or_error(response, "Radius device authorization").await?;
            let device_code = required_str(&value, "device_code")?;
            let user_code = required_str(&value, "user_code")?;
            let verification = trusted_http_url(&required_str(&value, "verification_uri")?)?;
            set_display(
                &display,
                verification.clone(),
                Some(user_code),
                "Open the Radius verification URL and enter the device code.",
            );
            open_url(&verification);
            device::poll(
                json_interval(&value),
                json_expires(&value),
                false,
                cancel_rx,
                || {
                    let client = client.clone();
                    let gateway = gateway.clone();
                    let device_code = device_code.clone();
                    async move {
                        let response = client
                            .post(format!("{gateway}/v1/oauth/token"))
                            .form_urlencoded(&[
                                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                                ("client_id", RADIUS_CLIENT_ID),
                                ("device_code", device_code.as_str()),
                            ])
                            .send()
                            .await?;
                        oauth_poll_from_token_response(response, "Radius").await
                    }
                },
            )
            .await
        }
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

pub(super) async fn refresh_radius(token: &OauthToken) -> Result<OauthToken> {
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

async fn read_token_json(response: reqwest::Response, label: &str) -> Result<OauthToken> {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{label} token request failed ({status}): {text}");
    }
    let value: Value = serde_json::from_str(&text).context(format!("{label} token is not JSON"))?;
    token_from_value(&value, label)
}

async fn read_token_form(response: reqwest::Response, label: &str) -> Result<OauthToken> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{label} token request failed ({status}): {value}");
    }
    token_from_value(&value, label)
}

async fn json_or_error(response: reqwest::Response, label: &str) -> Result<Value> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{label} failed ({status}): {value}");
    }
    Ok(value)
}

async fn oauth_poll_from_token_response(
    response: reqwest::Response,
    label: &str,
) -> Result<Poll<OauthToken>> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(Poll::Complete(token_from_value(&value, label)?));
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

fn token_from_value(value: &Value, _label: &str) -> Result<OauthToken> {
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

fn required_str(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing {field}"))
}

fn json_interval(value: &Value) -> Option<Duration> {
    value
        .get("interval")
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|value| *value > 0.0)
        .map(Duration::from_secs_f64)
}

fn json_expires(value: &Value) -> Option<Duration> {
    value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
}

trait FormUrlEncoded {
    fn form_urlencoded(self, fields: &[(&str, &str)]) -> Self;
}

impl FormUrlEncoded for reqwest::RequestBuilder {
    fn form_urlencoded(self, fields: &[(&str, &str)]) -> Self {
        apply_form(self, fields)
    }
}

fn apply_form(
    builder: reqwest::RequestBuilder,
    fields: &[(&str, &str)],
) -> reqwest::RequestBuilder {
    builder
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(fields))
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::getrandom(&mut buffer)
        .map_err(|error| anyhow!("cannot generate OAuth state: {error}"))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn anthropic_and_copilot_client_ids_match_pi() {
        assert_eq!(
            decode_b64(ANTHROPIC_CLIENT_ID_B64).unwrap(),
            "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
        );
        assert_eq!(
            decode_b64(COPILOT_CLIENT_ID_B64).unwrap(),
            "Iv1.b507a08c87ecfe98"
        );
    }

    #[test]
    fn github_domain_defaults_and_strips_scheme() {
        assert_eq!(
            normalize_github_domain(String::new()).unwrap(),
            "github.com"
        );
        assert_eq!(
            normalize_github_domain("https://company.ghe.com".into()).unwrap(),
            "company.ghe.com"
        );
    }

    #[test]
    fn extracts_chatgpt_account_id_from_access_token() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "account-123"
                }
            }))
            .unwrap(),
        );
        let token = format!("header.{payload}.signature");

        assert_eq!(chatgpt_account_id(&token).unwrap(), "account-123");
    }
}
