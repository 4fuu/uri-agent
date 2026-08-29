use super::super::util::{open_url, trusted_http_url};
use super::super::{LoginSetup, OauthLogin, OauthToken, channels, set_display};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;
use tokio::sync::{oneshot, watch};

const EXTERNAL_ENDPOINT: &str = "https://www.codebuddy.ai";
const INTERNAL_ENDPOINT: &str = "https://copilot.tencent.com";
const PLATFORM: &str = "CLI";
const PREFIX_PATH: &str = "/plugin";
const REFERENCE_VERSION: &str = "2.141.0";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_FETCH_TOKEN: i64 = 11217;
const RETRY_FETCH_ACCOUNT: i64 = 12151;
const ACCOUNT_AUTH_RETRIES: usize = 5;

pub(crate) const ENDPOINT_EXTRA: &str = "codebuddyEndpoint";
pub(crate) const ENVIRONMENT_EXTRA: &str = "codebuddyEnvironment";
pub(crate) const DOMAIN_EXTRA: &str = "codebuddyDomain";
pub(crate) const METHOD_EXTRA: &str = "codebuddyAuthMethod";
pub(crate) const ACCOUNT_EXTRA: &str = "codebuddyAccount";
pub(crate) const ACCOUNTS_EXTRA: &str = "codebuddyAccounts";
const AUTH_EXTRA: &str = "codebuddyAuth";

pub(in crate::oauth) fn start_codebuddy(
    environment: &str,
    endpoint: Option<&str>,
) -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let endpoint = login_endpoint(environment, endpoint)?;
    let environment = environment.to_string();
    let LoginSetup {
        login,
        paste_rx: _,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(
        endpoint.clone(),
        None,
        "Generating the CodeBuddy login URL…",
    );
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            LOGIN_TIMEOUT,
            codebuddy_login(endpoint, environment, cancel_rx, display),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("CodeBuddy login timed out after 5 minutes")),
        };
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

async fn codebuddy_login(
    endpoint: String,
    environment: String,
    mut cancel_rx: watch::Receiver<bool>,
    display: std::sync::Arc<std::sync::Mutex<super::super::OauthDisplay>>,
) -> Result<OauthToken> {
    let client = http_client()?;
    let domain = endpoint_domain(&endpoint)?;
    let state_url = api_url(
        &endpoint,
        &format!("/v2{PREFIX_PATH}/auth/state?platform={PLATFORM}"),
    );
    let response = send_or_cancel(
        &mut cancel_rx,
        client
            .post(state_url)
            .header("Accept", "application/json")
            .header("X-Domain", &domain)
            .header("X-No-Authorization", "true")
            .header("X-No-User-Id", "true")
            .header("X-No-Enterprise-Id", "true")
            .header("X-No-Department-Info", "true")
            .json(&json!({}))
            .send(),
    )
    .await
    .context("CodeBuddy auth state request failed")?;
    let state_reply = ProviderReply::read(response).await?;
    let state_payload = state_reply.payload("CodeBuddy auth state")?;
    let state = required_string(&state_payload, "state")?;
    let auth_url = decorate_auth_url(required_string(&state_payload, "authUrl")?)?;
    set_display(
        &display,
        auth_url.clone(),
        None,
        "Complete CodeBuddy sign-in in the browser. This window will continue polling.",
    );
    open_url(&auth_url);

    let auth = poll_token(&client, &endpoint, &state, &mut cancel_rx).await?;
    let access = required_string(&auth, "accessToken")?;
    let account = poll_account(
        &client,
        &endpoint,
        &state,
        &access,
        auth.get("domain")
            .and_then(Value::as_str)
            .unwrap_or(&domain),
        &mut cancel_rx,
    )
    .await?;
    let accounts = fetch_accounts(&client, &endpoint, &auth, false).await?;
    let account = merge_current_account(account, &accounts);
    token_from_session(auth, &endpoint, &environment, account, accounts)
}

async fn poll_token(
    client: &reqwest::Client,
    endpoint: &str,
    state: &str,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<Value> {
    let domain = endpoint_domain(endpoint)?;
    let url = api_url(
        endpoint,
        &format!("/v2{PREFIX_PATH}/auth/token?state={state}"),
    );
    loop {
        wait_to_poll(cancel_rx).await?;
        let response = send_or_cancel(
            cancel_rx,
            client
                .get(&url)
                .header("Accept", "application/json")
                .header("X-Domain", &domain)
                .header("X-No-Authorization", "true")
                .header("X-No-User-Id", "true")
                .header("X-No-Enterprise-Id", "true")
                .header("X-No-Department-Info", "true")
                .send(),
        )
        .await
        .context("CodeBuddy token polling failed")?;
        let reply = ProviderReply::read(response).await?;
        if reply.code() == Some(RETRY_FETCH_TOKEN) {
            continue;
        }
        let payload = reply.payload("CodeBuddy auth token")?;
        if payload
            .get("accessToken")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        {
            return Ok(payload);
        }
    }
}

async fn poll_account(
    client: &reqwest::Client,
    endpoint: &str,
    state: &str,
    access: &str,
    domain: &str,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<Value> {
    let url = api_url(
        endpoint,
        &format!("/v2{PREFIX_PATH}/login/account?state={state}"),
    );
    let mut auth_retries = 0;
    loop {
        wait_to_poll(cancel_rx).await?;
        let response = send_or_cancel(
            cancel_rx,
            client
                .get(&url)
                .header("Accept", "application/json")
                .header("Authorization", format!("Bearer {access}"))
                .header("X-Domain", domain)
                .header("X-No-User-Id", "true")
                .header("X-No-Enterprise-Id", "true")
                .header("X-No-Department-Info", "true")
                .send(),
        )
        .await
        .context("CodeBuddy account polling failed")?;
        let reply = ProviderReply::read(response).await?;
        if reply.code() == Some(RETRY_FETCH_ACCOUNT) {
            continue;
        }
        if matches!(
            reply.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) && auth_retries < ACCOUNT_AUTH_RETRIES
        {
            auth_retries += 1;
            continue;
        }
        let payload = reply.payload("CodeBuddy login account")?;
        if payload.get("uid").and_then(Value::as_str).is_some() {
            return Ok(payload);
        }
    }
}

pub(in crate::oauth) async fn refresh_codebuddy(token: &OauthToken) -> Result<OauthToken> {
    if token.refresh.is_empty() {
        bail!("CodeBuddy credential has no refresh token; run :login again");
    }
    let endpoint = extra_string(token, ENDPOINT_EXTRA)
        .or_else(|| {
            extra_string(token, ENVIRONMENT_EXTRA)
                .and_then(|environment| default_endpoint(&environment).map(str::to_string))
        })
        .ok_or_else(|| anyhow!("CodeBuddy credential has no endpoint; run :login again"))?;
    let endpoint = auth_endpoint(&endpoint)?;
    let environment = extra_string(token, ENVIRONMENT_EXTRA).unwrap_or_else(|| "external".into());
    let domain = extra_string(token, DOMAIN_EXTRA).unwrap_or(endpoint_domain(&endpoint)?);
    let client = http_client()?;
    let response = client
        .post(api_url(
            &endpoint,
            &format!("/v2{PREFIX_PATH}/auth/token/refresh"),
        ))
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", token.access))
        .header("X-Refresh-Token", &token.refresh)
        .header("X-Auth-Refresh-Source", "plugin")
        .header("X-Domain", &domain)
        .json(&json!({}))
        .send()
        .await
        .context("CodeBuddy token refresh failed")?;
    let reply = ProviderReply::read(response).await?;
    let mut auth = reply.payload("CodeBuddy token refresh")?;
    let auth_object = auth
        .as_object_mut()
        .ok_or_else(|| anyhow!("CodeBuddy token refresh returned no token"))?;
    auth_object
        .entry("domain")
        .or_insert_with(|| Value::String(domain));
    auth_object
        .entry("refreshToken")
        .or_insert_with(|| Value::String(token.refresh.clone()));
    let accounts = fetch_accounts(&client, &endpoint, &auth, true).await?;
    let account = accounts
        .iter()
        .find(|account| account.get("lastLogin").and_then(Value::as_bool) == Some(true))
        .or_else(|| accounts.first())
        .cloned()
        .ok_or_else(|| anyhow!("CodeBuddy refresh returned an empty account list"))?;
    token_from_session(auth, &endpoint, &environment, account, accounts)
}

async fn fetch_accounts(
    client: &reqwest::Client,
    endpoint: &str,
    auth: &Value,
    required: bool,
) -> Result<Vec<Value>> {
    let access = required_string(auth, "accessToken")?;
    let domain = auth
        .get("domain")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or(endpoint_domain(endpoint)?);
    let response = client
        .get(api_url(endpoint, &format!("/v2{PREFIX_PATH}/accounts")))
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {access}"))
        .header("X-Domain", domain)
        .send()
        .await
        .context("CodeBuddy account list request failed")?;
    let reply = ProviderReply::read(response).await?;
    let payload = reply.payload("CodeBuddy account list")?;
    let accounts = payload
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if required && accounts.is_empty() {
        bail!("CodeBuddy account list is empty");
    }
    Ok(accounts)
}

fn token_from_session(
    auth: Value,
    endpoint: &str,
    environment: &str,
    account: Value,
    accounts: Vec<Value>,
) -> Result<OauthToken> {
    let access = required_string(&auth, "accessToken")?;
    let refresh = auth
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expires = token_expiry(&auth, refresh.is_empty());
    let domain = auth
        .get("domain")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or(endpoint_domain(endpoint)?);
    let mut safe_auth = auth.as_object().cloned().unwrap_or_default();
    safe_auth.remove("accessToken");
    safe_auth.remove("refreshToken");
    let mut extra = BTreeMap::from([
        (
            ENDPOINT_EXTRA.to_string(),
            Value::String(endpoint.to_string()),
        ),
        (
            ENVIRONMENT_EXTRA.to_string(),
            Value::String(environment.to_string()),
        ),
        (DOMAIN_EXTRA.to_string(), Value::String(domain)),
        (ACCOUNT_EXTRA.to_string(), account),
        (ACCOUNTS_EXTRA.to_string(), Value::Array(accounts)),
        (AUTH_EXTRA.to_string(), Value::Object(safe_auth)),
    ]);
    if let Some(method) = auth
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
    {
        extra.insert(METHOD_EXTRA.to_string(), Value::String(method.to_string()));
    }
    Ok(OauthToken {
        kind: "oauth".to_string(),
        refresh,
        access,
        expires,
        extra,
    })
}

fn token_expiry(auth: &Value, nonrenewable: bool) -> i64 {
    if nonrenewable {
        return i64::MAX / 4;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let expires_at = auth
        .get("expiresAt")
        .and_then(Value::as_i64)
        .map(|value| {
            if value < 10_000_000_000 {
                value * 1000
            } else {
                value
            }
        })
        .or_else(|| {
            auth.get("expiresIn")
                .and_then(Value::as_i64)
                .filter(|value| *value > 0)
                .map(|seconds| now + seconds * 1000)
        })
        // CodeBuddy normally returns expiresAt or expiresIn. Preserve a
        // renewable session if an older enterprise deployment omits both;
        // forcing it immediately expired would create a refresh loop.
        .unwrap_or(now + 24 * 60 * 60 * 1000);
    expires_at.saturating_sub(5 * 60 * 1000)
}

fn merge_current_account(mut current: Value, accounts: &[Value]) -> Value {
    let uid = current
        .get("uid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let enterprise = current
        .get("enterpriseId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let selected = accounts.iter().find(|account| {
        account.get("uid").and_then(Value::as_str) == uid.as_deref()
            && account.get("enterpriseId").and_then(Value::as_str) == enterprise.as_deref()
    });
    let (Some(current), Some(selected)) =
        (current.as_object_mut(), selected.and_then(Value::as_object))
    else {
        return current;
    };
    current.extend(selected.clone());
    Value::Object(current.clone())
}

struct ProviderReply {
    status: StatusCode,
    value: Value,
}

impl ProviderReply {
    async fn read(response: reqwest::Response) -> Result<Self> {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Self { status, value })
    }

    fn code(&self) -> Option<i64> {
        self.value.get("code").and_then(|code| {
            code.as_i64()
                .or_else(|| code.as_str().and_then(|code| code.parse().ok()))
        })
    }

    fn payload(&self, label: &str) -> Result<Value> {
        let code = self.code();
        if !self.status.is_success() || code.is_some_and(|code| code != 0) {
            let message = self
                .value
                .get("msg")
                .or_else(|| self.value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("provider rejected the request");
            if matches!(code, Some(12005 | 11212 | 11216)) {
                bail!(
                    "{label} failed because the CodeBuddy license is unavailable ({code:?}): {message}"
                );
            }
            if code == Some(10081) {
                bail!("{label} failed because the current IP is not allowed");
            }
            bail!(
                "{label} failed ({}, code {:?}): {message}",
                self.status,
                code
            );
        }
        Ok(self
            .value
            .get("data")
            .cloned()
            .unwrap_or_else(|| self.value.clone()))
    }
}

async fn send_or_cancel<T>(
    cancel_rx: &mut watch::Receiver<bool>,
    future: impl Future<Output = Result<T, reqwest::Error>>,
) -> Result<T> {
    if *cancel_rx.borrow() {
        bail!("CodeBuddy login was cancelled");
    }
    tokio::select! {
        result = future => Ok(result?),
        changed = cancel_rx.changed() => {
            let _ = changed;
            bail!("CodeBuddy login was cancelled")
        }
    }
}

async fn wait_to_poll(cancel_rx: &mut watch::Receiver<bool>) -> Result<()> {
    if *cancel_rx.borrow() {
        bail!("CodeBuddy login was cancelled");
    }
    tokio::select! {
        () = tokio::time::sleep(POLL_INTERVAL) => Ok(()),
        changed = cancel_rx.changed() => {
            let _ = changed;
            bail!("CodeBuddy login was cancelled")
        }
    }
}

fn http_client() -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-requested-with",
        "XMLHttpRequest".parse().expect("static header value"),
    );
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("CodeBuddy/{REFERENCE_VERSION}"))
        .default_headers(headers)
        .build()?)
}

fn login_endpoint(environment: &str, endpoint: Option<&str>) -> Result<String> {
    match environment {
        "external" | "internal" => normalize_endpoint(
            default_endpoint(environment).expect("built-in CodeBuddy environment has endpoint"),
        ),
        "selfhosted" => {
            let value = endpoint
                .map(str::to_string)
                .or_else(|| std::env::var("CODEBUDDY_BASE_URL").ok())
                .ok_or_else(|| anyhow!("CodeBuddy Enterprise Domain requires an endpoint"))?;
            auth_endpoint(&value)
        }
        "iOA" | "ioa" => bail!("CodeBuddy iOA login is not supported"),
        other => bail!("unsupported CodeBuddy login environment {other:?}"),
    }
}

pub(crate) fn default_endpoint(environment: &str) -> Option<&'static str> {
    match environment {
        "external" => Some(EXTERNAL_ENDPOINT),
        "internal" => Some(INTERNAL_ENDPOINT),
        _ => None,
    }
}

pub(crate) fn normalize_endpoint(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/');
    let url = Url::parse(value).context("invalid CodeBuddy endpoint")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("CodeBuddy endpoint must be an HTTP or HTTPS URL");
    }
    Ok(value.to_string())
}

fn auth_endpoint(value: &str) -> Result<String> {
    let value = normalize_endpoint(value)?;
    Ok(value.strip_suffix("/v2").unwrap_or(&value).to_string())
}

fn endpoint_domain(endpoint: &str) -> Result<String> {
    let url = Url::parse(endpoint).context("invalid CodeBuddy endpoint")?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("CodeBuddy endpoint has no domain"))?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(url
        .port()
        .map_or(host.clone(), |port| format!("{host}:{port}")))
}

fn api_url(endpoint: &str, path: &str) -> String {
    format!("{}{path}", endpoint.trim_end_matches('/'))
}

fn decorate_auth_url(value: String) -> Result<String> {
    let value = trusted_http_url(&value)?;
    let mut url = Url::parse(&value).context("invalid CodeBuddy login URL")?;
    url.query_pairs_mut()
        .append_pair("version", REFERENCE_VERSION);
    Ok(url.to_string())
}

fn required_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("CodeBuddy response has no {key}"))
}

fn extra_string(token: &OauthToken, key: &str) -> Option<String> {
    token
        .extra
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

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
    fn login_environments_use_the_reference_endpoints() {
        assert_eq!(login_endpoint("external", None).unwrap(), EXTERNAL_ENDPOINT);
        assert_eq!(login_endpoint("internal", None).unwrap(), INTERNAL_ENDPOINT);
        assert!(login_endpoint("iOA", None).is_err());
        assert!(login_endpoint("unknown", None).is_err());
        assert_eq!(
            login_endpoint("selfhosted", Some("http://localhost:3000/")).unwrap(),
            "http://localhost:3000"
        );
        assert_eq!(
            login_endpoint("selfhosted", Some("http://localhost:3000/v2/")).unwrap(),
            "http://localhost:3000"
        );
    }

    #[test]
    fn token_session_keeps_identity_without_duplicating_secrets() {
        let token = token_from_session(
            json!({
                "accessToken": "access",
                "refreshToken": "refresh",
                "expiresIn": 3600,
                "domain": "www.codebuddy.ai",
                "method": "github"
            }),
            EXTERNAL_ENDPOINT,
            "external",
            json!({"uid": "user-1", "enterpriseId": "enterprise-1"}),
            vec![json!({"uid": "user-1", "enterpriseId": "enterprise-1"})],
        )
        .unwrap();

        assert_eq!(token.access, "access");
        assert_eq!(token.refresh, "refresh");
        assert_eq!(token.extra[DOMAIN_EXTRA], "www.codebuddy.ai");
        assert_eq!(token.extra[METHOD_EXTRA], "github");
        assert!(token.extra[AUTH_EXTRA].get("accessToken").is_none());
        assert!(token.extra[AUTH_EXTRA].get("refreshToken").is_none());
    }

    #[test]
    fn provider_reply_unwraps_codebuddy_data_and_pending_codes() {
        let reply = ProviderReply {
            status: StatusCode::OK,
            value: json!({"code": 0, "data": {"state": "one"}}),
        };
        assert_eq!(reply.payload("test").unwrap()["state"], "one");
        let pending = ProviderReply {
            status: StatusCode::OK,
            value: json!({"code": RETRY_FETCH_TOKEN, "msg": "pending"}),
        };
        assert_eq!(pending.code(), Some(RETRY_FETCH_TOKEN));
        assert!(pending.payload("test").is_err());
    }

    #[test]
    fn browser_url_includes_the_reference_client_version() {
        let url =
            decorate_auth_url("https://www.codebuddy.ai/login?state=one".to_string()).unwrap();
        let url = Url::parse(&url).unwrap();
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "version" && value == REFERENCE_VERSION)
        );
    }

    #[tokio::test]
    async fn refresh_uses_codebuddy_headers_and_selects_last_login_account() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in [
                json!({
                    "code": 0,
                    "data": {
                        "accessToken": "fresh-access",
                        "refreshToken": "rotated-refresh",
                        "expiresIn": 3600,
                        "domain": "enterprise.example",
                        "method": "github"
                    }
                })
                .to_string(),
                json!({
                    "code": 0,
                    "data": {
                        "accounts": [
                            {"uid": "first", "enterpriseId": "one", "lastLogin": false},
                            {"uid": "selected", "enterpriseId": "two", "lastLogin": true}
                        ]
                    }
                })
                .to_string(),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut socket).await);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let endpoint = format!("http://{address}");
        let token = OauthToken {
            kind: "oauth".to_string(),
            refresh: "old-refresh".to_string(),
            access: "old-access".to_string(),
            expires: 0,
            extra: BTreeMap::from([
                (ENDPOINT_EXTRA.to_string(), Value::String(endpoint)),
                (
                    ENVIRONMENT_EXTRA.to_string(),
                    Value::String("selfhosted".to_string()),
                ),
                (
                    DOMAIN_EXTRA.to_string(),
                    Value::String("enterprise.example".to_string()),
                ),
            ]),
        };

        let refreshed = refresh_codebuddy(&token).await.unwrap();
        assert_eq!(refreshed.access, "fresh-access");
        assert_eq!(refreshed.refresh, "rotated-refresh");
        assert_eq!(refreshed.extra[ACCOUNT_EXTRA]["uid"], "selected");
        assert_eq!(refreshed.extra[METHOD_EXTRA], "github");
        assert!(refreshed.extra[AUTH_EXTRA].get("accessToken").is_none());
        assert!(refreshed.extra[AUTH_EXTRA].get("refreshToken").is_none());

        let requests = server.await.unwrap();
        let refresh = requests[0].to_ascii_lowercase();
        assert!(refresh.starts_with("post /v2/plugin/auth/token/refresh http/1.1"));
        assert!(refresh.contains("authorization: bearer old-access"));
        assert!(refresh.contains("x-refresh-token: old-refresh"));
        assert!(refresh.contains("x-auth-refresh-source: plugin"));
        assert!(refresh.contains("x-domain: enterprise.example"));
        assert!(refresh.contains("x-requested-with: xmlhttprequest"));
        assert!(refresh.ends_with("{}"));
        let accounts = requests[1].to_ascii_lowercase();
        assert!(accounts.starts_with("get /v2/plugin/accounts http/1.1"));
        assert!(accounts.contains("authorization: bearer fresh-access"));
        assert!(accounts.contains("x-domain: enterprise.example"));
        assert!(accounts.contains("x-requested-with: xmlhttprequest"));
    }
}
