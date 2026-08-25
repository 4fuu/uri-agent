use super::super::callback;
use super::super::util::{encode, generate_pkce, http_client, open_url};
use super::super::{LoginSetup, OauthLogin, OauthToken, channels};
use super::shared::{FormUrlEncoded, random_hex, read_token_form, token_from_value};
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
const DEFAULT_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const DEFAULT_CLIENT_SECRET: &str = "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";
const DEFAULT_USER_AGENT_VERSION: &str = "4.3.0";
const CONTROL_BASE_URLS: [&str; 3] = [
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
const SCOPES: &str = "openid https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
const EXTRA_EXPIRY_SKEW_MILLIS: i64 = 10 * 60 * 1000;
const OAUTH_CLIENT_ID_EXTRA: &str = "oauthClientId";

struct ClientIdentity {
    client_id: String,
    client_secret: String,
    oauth_user_agent: String,
}

pub(in crate::oauth) fn start_antigravity()
-> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let identity = client_identity();
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
    let identity = client_identity();
    validate_client_identity(token, &identity)?;
    let mut refreshed = with_antigravity_expiry_skew(
        request_refresh_token(&identity, &token.refresh, TOKEN_URL).await?,
    );
    refreshed.extra.clone_from(&token.extra);
    remember_client_identity(&mut refreshed, &identity);
    match enrich_token(refreshed.clone(), &identity, false).await {
        Ok(token) => Ok(token),
        Err(_) if project_id(&token.extra).is_some() => Ok(refreshed),
        Err(error) => Err(error),
    }
}

fn client_identity() -> ClientIdentity {
    let configured = |name: &str| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let version = configured("ANTIGRAVITY_USER_AGENT_VERSION")
        .unwrap_or_else(|| DEFAULT_USER_AGENT_VERSION.to_string());
    ClientIdentity {
        client_id: configured("ANTIGRAVITY_OAUTH_CLIENT_ID")
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string()),
        client_secret: configured("ANTIGRAVITY_OAUTH_CLIENT_SECRET")
            .unwrap_or_else(|| DEFAULT_CLIENT_SECRET.to_string()),
        oauth_user_agent: format!("vscode/1.X.X (Antigravity/{version})"),
    }
}

async fn exchange_code(
    identity: &ClientIdentity,
    code: &str,
    verifier: &str,
) -> Result<OauthToken> {
    let response = http_client()?
        .post(TOKEN_URL)
        .header("User-Agent", &identity.oauth_user_agent)
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
    let mut token = with_antigravity_expiry_skew(read_token_form(response, "Antigravity").await?);
    remember_client_identity(&mut token, identity);
    Ok(token)
}

async fn request_refresh_token(
    identity: &ClientIdentity,
    refresh: &str,
    token_url: &str,
) -> Result<OauthToken> {
    let client = http_client()?;
    for attempt in 0..=1 {
        let response = client
            .post(token_url)
            .header("User-Agent", &identity.oauth_user_agent)
            .form_urlencoded(&[
                ("client_id", &identity.client_id),
                ("client_secret", &identity.client_secret),
                ("refresh_token", refresh),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .context("Antigravity OAuth refresh failed")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status.is_success() {
            let value = serde_json::from_str(&text).context("Antigravity token is not JSON")?;
            return token_from_value(&value);
        }
        if attempt == 0 && text.contains("invalid_grant") {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        bail!("Antigravity token request failed ({status}): {text}");
    }
    unreachable!("Antigravity invalid_grant confirmation returns after its second attempt")
}

fn validate_client_identity(token: &OauthToken, identity: &ClientIdentity) -> Result<()> {
    let Some(stored) = token.extra.get(OAUTH_CLIENT_ID_EXTRA) else {
        return Ok(());
    };
    let stored = stored
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Antigravity OAuth credential has invalid client metadata"))?;
    if stored != identity.client_id {
        bail!(
            "Antigravity OAuth client differs from the client that issued this credential; restore ANTIGRAVITY_OAUTH_CLIENT_ID and ANTIGRAVITY_OAUTH_CLIENT_SECRET, or run :login and sign in again"
        );
    }
    Ok(())
}

fn remember_client_identity(token: &mut OauthToken, identity: &ClientIdentity) {
    token.extra.insert(
        OAUTH_CLIENT_ID_EXTRA.to_string(),
        Value::String(identity.client_id.clone()),
    );
}

fn with_antigravity_expiry_skew(mut token: OauthToken) -> OauthToken {
    token.expires = token.expires.saturating_sub(EXTRA_EXPIRY_SKEW_MILLIS);
    token
}

async fn enrich_token(
    mut token: OauthToken,
    identity: &ClientIdentity,
    require_project: bool,
) -> Result<OauthToken> {
    if let Ok(user) = http_client()?
        .get(USER_INFO_URL)
        .header("User-Agent", &identity.oauth_user_agent)
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

    match discover_project(&token.access, &identity.oauth_user_agent).await {
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
        .split("Antigravity/")
        .nth(1)
        .and_then(|value| value.split([')', ' ']).next())
        .filter(|value| !value.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn test_identity(client_id: &str) -> ClientIdentity {
        ClientIdentity {
            client_id: client_id.to_string(),
            client_secret: "test-secret".to_string(),
            oauth_user_agent: "vscode/1.X.X (Antigravity/test)".to_string(),
        }
    }

    fn test_token() -> OauthToken {
        OauthToken {
            kind: "oauth".to_string(),
            refresh: "old-refresh".to_string(),
            access: "old-access".to_string(),
            expires: 0,
            extra: BTreeMap::new(),
        }
    }

    async fn read_request(socket: &mut TcpStream) -> String {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0, "client closed before finishing its request");
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .map(|index| index + 4)
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or_default();
            if request.len() >= header_end + content_length {
                return String::from_utf8(request).unwrap();
            }
        }
    }

    #[test]
    fn reference_oauth_identity_has_direct_login_defaults() {
        assert_eq!(
            DEFAULT_CLIENT_ID,
            "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
        );
        assert!(!DEFAULT_CLIENT_SECRET.is_empty());
        assert_eq!(DEFAULT_USER_AGENT_VERSION, "4.3.0");
    }

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
        assert_eq!(ide_version("vscode/1.X.X (Antigravity/4.3.0)"), "4.3.0");
    }

    #[test]
    fn oauth_client_identity_is_persisted_without_its_secret_and_validated() {
        let issuing_identity = test_identity("issuing-client");
        let mut token = test_token();
        remember_client_identity(&mut token, &issuing_identity);
        assert_eq!(
            token.extra.get(OAUTH_CLIENT_ID_EXTRA),
            Some(&Value::String("issuing-client".to_string()))
        );
        let serialized = serde_json::to_string(&token).unwrap();
        assert!(!serialized.contains("test-secret"));
        validate_client_identity(&token, &issuing_identity).unwrap();

        let error = validate_client_identity(&token, &test_identity("other-client")).unwrap_err();
        assert!(error.to_string().contains("run :login"));

        token.extra.remove(OAUTH_CLIENT_ID_EXTRA);
        validate_client_identity(&token, &test_identity("legacy-client")).unwrap();
    }

    #[tokio::test]
    async fn invalid_grant_is_confirmed_once_with_the_same_client() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in [
                (400, r#"{"error":"invalid_grant"}"#),
                (
                    200,
                    r#"{"access_token":"fresh-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
                ),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut socket).await);
                let response = format!(
                    "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });

        let token = request_refresh_token(
            &test_identity("issuing-client"),
            "old-refresh",
            &format!("http://{address}/token"),
        )
        .await
        .unwrap();
        assert_eq!(token.access, "fresh-access");
        assert_eq!(token.refresh, "rotated-refresh");
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.contains("client_id=issuing-client"))
        );
        assert!(
            requests
                .iter()
                .all(|request| request.contains("refresh_token=old-refresh"))
        );
    }
}
