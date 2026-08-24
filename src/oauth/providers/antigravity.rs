use super::super::callback;
use super::super::util::{encode, generate_pkce, http_client, open_url};
use super::super::{LoginSetup, OauthLogin, OauthToken, channels};
use super::shared::{FormUrlEncoded, random_hex, read_token_form};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::time::Duration;
use tokio::sync::oneshot;

const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USER_INFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
const REDIRECT_URI: &str = "http://localhost:8085/callback";
const CONTROL_BASE_URLS: [&str; 2] = [
    "https://cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.googleapis.com",
];
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";

struct ClientIdentity {
    client_id: String,
    client_secret: String,
    user_agent: String,
}

pub(in crate::oauth) fn start_antigravity()
-> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let identity = client_identity()?;
    let pkce = generate_pkce()?;
    let state = random_hex(24)?;
    let url = format!(
        "{AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent&include_granted_scopes=true",
        encode(&identity.client_id),
        encode(REDIRECT_URI),
        encode(SCOPES),
        encode(&pkce.challenge),
        encode(&state),
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
        "Experimental private protocol. Complete Google sign-in, or paste the redirect URL / code.",
    );
    open_url(&url);
    tokio::spawn(async move {
        let result = async {
            let callback = callback::bind(
                "127.0.0.1",
                8085,
                "/callback",
                REDIRECT_URI,
                Some(&state),
                "Antigravity authentication completed. You can close this window.",
            )
            .await;
            let (code, _) = callback::race_callback_or_paste(
                &callback,
                &mut paste_rx,
                &mut cancel_rx,
                Some(&state),
            )
            .await?;
            let token = exchange_code(&identity, &code, &pkce.verifier).await?;
            enrich_token(token, &identity, true).await
        }
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

pub(in crate::oauth) async fn refresh_antigravity(token: &OauthToken) -> Result<OauthToken> {
    let identity = client_identity()?;
    let response = http_client()?
        .post(TOKEN_URL)
        .form_urlencoded(&[
            ("client_id", &identity.client_id),
            ("client_secret", &identity.client_secret),
            ("refresh_token", &token.refresh),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .context("Antigravity OAuth refresh failed")?;
    let mut refreshed = read_token_form(response, "Antigravity").await?;
    if refreshed.refresh.is_empty() {
        refreshed.refresh.clone_from(&token.refresh);
    }
    refreshed.extra.clone_from(&token.extra);
    match enrich_token(refreshed.clone(), &identity, false).await {
        Ok(token) => Ok(token),
        Err(_) if project_id(&token.extra).is_some() => Ok(refreshed),
        Err(error) => Err(error),
    }
}

fn client_identity() -> Result<ClientIdentity> {
    let required = |name: &str| {
        env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("{name} is required for experimental Antigravity OAuth"))
    };
    Ok(ClientIdentity {
        client_id: required("ANTIGRAVITY_OAUTH_CLIENT_ID")?,
        client_secret: required("ANTIGRAVITY_OAUTH_CLIENT_SECRET")?,
        user_agent: required("ANTIGRAVITY_USER_AGENT")?,
    })
}

async fn exchange_code(
    identity: &ClientIdentity,
    code: &str,
    verifier: &str,
) -> Result<OauthToken> {
    let response = http_client()?
        .post(TOKEN_URL)
        .form_urlencoded(&[
            ("client_id", &identity.client_id),
            ("client_secret", &identity.client_secret),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("Antigravity OAuth token exchange failed")?;
    read_token_form(response, "Antigravity").await
}

async fn enrich_token(
    mut token: OauthToken,
    identity: &ClientIdentity,
    require_project: bool,
) -> Result<OauthToken> {
    token.extra.insert(
        "userAgent".to_string(),
        Value::String(identity.user_agent.clone()),
    );
    if let Ok(user) = http_client()?
        .get(USER_INFO_URL)
        .bearer_auth(&token.access)
        .send()
        .await
        && user.status().is_success()
        && let Ok(value) = user.json::<Value>().await
        && let Some(email) = value.get("email").and_then(Value::as_str)
    {
        token
            .extra
            .insert("email".to_string(), Value::String(email.to_string()));
    }

    match discover_project(&token.access, &identity.user_agent).await {
        Ok(project) => {
            token
                .extra
                .insert("projectId".to_string(), Value::String(project.id));
            if let Some(tier) = project.tier {
                token.extra.insert("tier".to_string(), Value::String(tier));
            }
        }
        Err(error) if require_project || project_id(&token.extra).is_none() => return Err(error),
        Err(_) => {}
    }
    Ok(token)
}

struct Project {
    id: String,
    tier: Option<String>,
}

async fn discover_project(access: &str, user_agent: &str) -> Result<Project> {
    let mut last_error = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1 << (attempt - 1))).await;
        }
        match load_code_assist(access, user_agent).await {
            Ok((value, base_url)) => {
                let tier = tier_id(&value);
                if let Some(id) = extract_project_id(&value) {
                    return Ok(Project { id, tier });
                }
                let default_tier = default_tier_id(&value).ok_or_else(|| {
                    anyhow!("Antigravity did not return a project or default tier")
                })?;
                match onboard_user(access, user_agent, &base_url, &default_tier).await {
                    Ok(id) => return Ok(Project { id, tier }),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("Antigravity project discovery failed")))
}

async fn load_code_assist(access: &str, user_agent: &str) -> Result<(Value, String)> {
    control_request(
        access,
        user_agent,
        "loadCodeAssist",
        json!({
            "metadata": {
                "ideType": "ANTIGRAVITY",
                "ideName": "antigravity",
                "ideVersion": ide_version(user_agent)
            }
        }),
        &CONTROL_BASE_URLS,
    )
    .await
}

async fn onboard_user(
    access: &str,
    user_agent: &str,
    preferred_base_url: &str,
    tier: &str,
) -> Result<String> {
    let mut bases = vec![preferred_base_url];
    bases.extend(
        CONTROL_BASE_URLS
            .iter()
            .copied()
            .filter(|base| *base != preferred_base_url),
    );
    for _ in 0..5 {
        let (value, _) = control_request(
            access,
            user_agent,
            "onboardUser",
            json!({
                "tierId": tier,
                "metadata": {
                    "ideType": "ANTIGRAVITY",
                    "platform": "PLATFORM_UNSPECIFIED",
                    "pluginType": "GEMINI"
                }
            }),
            &bases,
        )
        .await?;
        if value.get("done").and_then(Value::as_bool) == Some(true) {
            return extract_project_id(value.get("response").unwrap_or(&Value::Null))
                .ok_or_else(|| anyhow!("Antigravity onboarding completed without a project"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("Antigravity onboarding did not complete")
}

async fn control_request(
    access: &str,
    user_agent: &str,
    action: &str,
    body: Value,
    base_urls: &[&str],
) -> Result<(Value, String)> {
    let client = http_client()?;
    let mut last_error = None;
    for (index, base_url) in base_urls.iter().enumerate() {
        let response = match client
            .post(format!("{base_url}/v1internal:{action}"))
            .bearer_auth(access)
            .header("User-Agent", user_agent)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if index + 1 < base_urls.len() => {
                last_error = Some(anyhow!(error));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let fallback = matches!(status.as_u16(), 404 | 408 | 429) || status.is_server_error();
        if !status.is_success() {
            let error = anyhow!("Antigravity {action} failed ({status}): {text}");
            if fallback && index + 1 < base_urls.len() {
                last_error = Some(error);
                continue;
            }
            return Err(error);
        }
        let value = serde_json::from_str(&text)
            .with_context(|| format!("Antigravity {action} response is not JSON"))?;
        return Ok((value, (*base_url).to_string()));
    }
    Err(last_error.unwrap_or_else(|| anyhow!("Antigravity {action} failed")))
}

fn project_id(extra: &BTreeMap<String, Value>) -> Option<&str> {
    extra
        .get("projectId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn extract_project_id(value: &Value) -> Option<String> {
    let value = value.get("cloudaicompanionProject")?;
    value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn tier_id(value: &Value) -> Option<String> {
    ["paidTier", "currentTier"]
        .into_iter()
        .find_map(|key| {
            let tier = value.get(key)?;
            tier.as_str()
                .or_else(|| tier.get("id").and_then(Value::as_str))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn default_tier_id(value: &Value) -> Option<String> {
    value
        .get("allowedTiers")?
        .as_array()?
        .iter()
        .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))?
        .get("id")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn ide_version(user_agent: &str) -> &str {
    user_agent
        .strip_prefix("antigravity/")
        .and_then(|value| value.split_whitespace().next())
        .filter(|value| !value.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_and_tier_accept_reference_response_shapes() {
        let object = json!({
            "cloudaicompanionProject": {"id": "project-object"},
            "paidTier": {"id": "g1-pro-tier"}
        });
        assert_eq!(
            extract_project_id(&object).as_deref(),
            Some("project-object")
        );
        assert_eq!(tier_id(&object).as_deref(), Some("g1-pro-tier"));

        let strings = json!({
            "cloudaicompanionProject": "project-string",
            "currentTier": "free-tier"
        });
        assert_eq!(
            extract_project_id(&strings).as_deref(),
            Some("project-string")
        );
        assert_eq!(tier_id(&strings).as_deref(), Some("free-tier"));
    }

    #[test]
    fn default_tier_requires_explicit_default_marker() {
        let response = json!({
            "allowedTiers": [
                {"id": "other", "isDefault": false},
                {"id": "free-tier", "isDefault": true}
            ]
        });
        assert_eq!(default_tier_id(&response).as_deref(), Some("free-tier"));
    }

    #[test]
    fn ide_version_comes_from_the_explicit_user_agent() {
        assert_eq!(ide_version("antigravity/1.23.2 windows/amd64"), "1.23.2");
    }
}
