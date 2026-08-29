use super::super::util::{open_url, trusted_http_url};
use super::super::{LoginSetup, OauthDisplay, OauthLogin, OauthToken, channels, set_display};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{StatusCode, Url};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;
use tokio::sync::{oneshot, watch};

const WORKBUDDY_ENDPOINT: &str = "https://copilot.tencent.com";
const PLATFORM: &str = "workbuddy";
const PREFIX_PATH: &str = "/plugin";
const REFERENCE_VERSION: &str = "5.3.14";
pub(crate) const USER_AGENT: &str = "WorkBuddy/5.3.14 WorkBuddy/5.3.14 CLI/2.115.0";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_FETCH_TOKEN: i64 = 11217;
const RETRY_FETCH_ACCOUNT: i64 = 12151;
const ACCOUNT_AUTH_RETRIES: usize = 5;

pub(crate) const ENDPOINT_EXTRA: &str = "workbuddyEndpoint";
pub(crate) const ENVIRONMENT_EXTRA: &str = "workbuddyEnvironment";
pub(crate) const DOMAIN_EXTRA: &str = "workbuddyDomain";
pub(crate) const METHOD_EXTRA: &str = "workbuddyAuthMethod";
pub(crate) const ACCOUNT_EXTRA: &str = "workbuddyAccount";
pub(crate) const ACCOUNTS_EXTRA: &str = "workbuddyAccounts";
const AUTH_EXTRA: &str = "workbuddyAuth";

pub(crate) const WORKBUDDY_ENVIRONMENT_VARIABLE: &str = "CODEBUDDY_INTERNET_ENVIRONMENT";
pub(crate) const WORKBUDDY_BASE_URL_VARIABLE: &str = "CODEBUDDY_BASE_URL";
pub(crate) const WORKBUDDY_AUTH_TOKEN_VARIABLE: &str = "CODEBUDDY_AUTH_TOKEN";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorkBuddySession {
    pub endpoint: String,
    pub domain: Option<String>,
    pub method: Option<String>,
    pub account: Option<Value>,
}

pub(crate) fn workbuddy_session_from_oauth(
    token: &OauthToken,
    base_url: Option<&str>,
) -> Result<WorkBuddySession> {
    let environment =
        extra_string(token, ENVIRONMENT_EXTRA).unwrap_or_else(|| "internal".to_string());
    let endpoint = base_url
        .map(str::to_string)
        .or_else(|| extra_string(token, ENDPOINT_EXTRA))
        .or_else(|| default_endpoint(&environment).map(str::to_string))
        .ok_or_else(|| anyhow!("WorkBuddy OAuth session has no endpoint; run :login again"))?;
    Ok(WorkBuddySession {
        endpoint: normalize_endpoint(&endpoint)?,
        domain: extra_string(token, DOMAIN_EXTRA),
        method: extra_string(token, METHOD_EXTRA),
        account: token.extra.get(ACCOUNT_EXTRA).cloned(),
    })
}

pub(crate) fn process_workbuddy_session(
    base_url: Option<String>,
    environment: Option<String>,
) -> Result<WorkBuddySession> {
    let endpoint = if let Some(value) = base_url.filter(|value| !value.trim().is_empty()) {
        normalize_endpoint(&value)?
    } else {
        let environment = environment
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "internal".to_string());
        match environment.as_str() {
            "internal" => default_endpoint(&environment)
                .expect("the built-in WorkBuddy environment has an endpoint")
                .to_string(),
            "selfhosted" => {
                bail!("a custom WorkBuddy endpoint requires {WORKBUDDY_BASE_URL_VARIABLE}")
            }
            "external" | "iOA" | "ioa" => {
                bail!("the {environment} login environment is not supported by WorkBuddy")
            }
            other => bail!("unsupported WorkBuddy environment {other:?}"),
        }
    };
    Ok(WorkBuddySession {
        endpoint,
        domain: None,
        method: None,
        account: None,
    })
}

pub(crate) fn workbuddy_authenticated_headers(
    token: &str,
    api_key: bool,
    session: &WorkBuddySession,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    insert_header(
        &mut headers,
        http::header::AUTHORIZATION,
        &format!("Bearer {token}"),
        "WorkBuddy bearer token",
    )?;
    headers.insert(
        HeaderName::from_static("x-requested-with"),
        HeaderValue::from_static("XMLHttpRequest"),
    );
    headers.insert(
        HeaderName::from_static("x-product"),
        HeaderValue::from_static("SaaS"),
    );
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    if api_key {
        insert_header(
            &mut headers,
            HeaderName::from_static("x-api-key"),
            token,
            "WorkBuddy API key",
        )?;
    }
    apply_session_headers(&mut headers, session)?;
    Ok(headers)
}

fn apply_session_headers(headers: &mut HeaderMap, session: &WorkBuddySession) -> Result<()> {
    if let Some(domain) = &session.domain {
        insert_static(headers, "x-domain", domain, "WorkBuddy domain")?;
    }
    let Some(account) = session.account.as_ref() else {
        return Ok(());
    };
    let uid = value_string(account, "uid");
    let enterprise = value_string(account, "enterpriseId");
    let department = value_string(account, "departmentFullName");
    let id_source = value_string(account, "idSource");
    if let Some(uid) = uid {
        insert_static(headers, "x-user-id", uid, "WorkBuddy user ID")?;
    }
    if let Some(enterprise) = enterprise {
        insert_static(
            headers,
            "x-enterprise-id",
            enterprise,
            "WorkBuddy enterprise ID",
        )?;
        insert_static(headers, "x-tenant-id", enterprise, "WorkBuddy tenant ID")?;
    }
    if let Some(department) = department {
        insert_static(
            headers,
            "x-department-info",
            department,
            "WorkBuddy department",
        )?;
    }
    if let Some(method) = &session.method {
        insert_static(headers, "x-auth-method", method, "WorkBuddy auth method")?;
    }
    if let Some(id_source) = id_source {
        insert_static(
            headers,
            "x-id-source",
            id_source,
            "WorkBuddy identity source",
        )?;
    }
    if let Some(uid) = uid
        && (enterprise.is_some() || id_source.is_some() || session.method.is_some())
    {
        let mut userinfo = Map::from_iter([("uin".to_string(), Value::String(uid.to_string()))]);
        if let Some(enterprise) = enterprise {
            userinfo.insert(
                "owner_uin".to_string(),
                Value::String(enterprise.to_string()),
            );
        }
        if let Some(id_source) = id_source {
            userinfo.insert(
                "id_source".to_string(),
                Value::String(id_source.to_string()),
            );
        }
        if let Some(method) = &session.method {
            userinfo.insert(
                "token_source".to_string(),
                Value::String(method.to_string()),
            );
        }
        insert_static(
            headers,
            "x-userinfo",
            &BASE64.encode(serde_json::to_vec(&userinfo)?),
            "WorkBuddy encoded user info",
        )?;
    }
    Ok(())
}

fn value_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn insert_static(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
    label: &str,
) -> Result<()> {
    insert_header(headers, HeaderName::from_static(name), value, label)
}

fn insert_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
    label: &str,
) -> Result<()> {
    headers.insert(
        name,
        HeaderValue::from_str(value).with_context(|| format!("invalid {label} header value"))?,
    );
    Ok(())
}

pub(crate) fn start_login() -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let endpoint = WORKBUDDY_ENDPOINT.to_string();
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
        "Generating the WorkBuddy login URL…",
    );
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            LOGIN_TIMEOUT,
            workbuddy_login(endpoint, cancel_rx, display),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("WorkBuddy login timed out after 5 minutes")),
        };
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

async fn workbuddy_login(
    endpoint: String,
    mut cancel_rx: watch::Receiver<bool>,
    display: std::sync::Arc<std::sync::Mutex<OauthDisplay>>,
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
    .context("WorkBuddy auth state request failed")?;
    let state_reply = ProviderReply::read(response).await?;
    let state_payload = state_reply.payload("WorkBuddy auth state")?;
    let state = required_string(&state_payload, "state")?;
    let auth_url = decorate_auth_url(required_string(&state_payload, "authUrl")?)?;
    set_display(
        &display,
        auth_url.clone(),
        None,
        "Complete WorkBuddy sign-in in the browser. This window will continue polling.",
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
    token_from_session(auth, &endpoint, "internal", account, accounts)
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
        .context("WorkBuddy token polling failed")?;
        let reply = ProviderReply::read(response).await?;
        if reply.code() == Some(RETRY_FETCH_TOKEN) {
            continue;
        }
        let payload = reply.payload("WorkBuddy auth token")?;
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
        .context("WorkBuddy account polling failed")?;
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
        let payload = reply.payload("WorkBuddy login account")?;
        if payload.get("uid").and_then(Value::as_str).is_some() {
            return Ok(payload);
        }
    }
}

pub(crate) async fn refresh_token(token: &OauthToken) -> Result<OauthToken> {
    if token.refresh.is_empty() {
        bail!("WorkBuddy credential has no refresh token; run :login again");
    }
    let endpoint = extra_string(token, ENDPOINT_EXTRA)
        .or_else(|| {
            extra_string(token, ENVIRONMENT_EXTRA)
                .and_then(|environment| default_endpoint(&environment).map(str::to_string))
        })
        .ok_or_else(|| anyhow!("WorkBuddy credential has no endpoint; run :login again"))?;
    let endpoint = auth_endpoint(&endpoint)?;
    let environment = extra_string(token, ENVIRONMENT_EXTRA).unwrap_or_else(|| "internal".into());
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
        .context("WorkBuddy token refresh failed")?;
    let reply = ProviderReply::read(response).await?;
    let mut auth = reply.payload("WorkBuddy token refresh")?;
    let auth_object = auth
        .as_object_mut()
        .ok_or_else(|| anyhow!("WorkBuddy token refresh returned no token"))?;
    auth_object
        .entry("domain")
        .or_insert_with(|| Value::String(domain));
    auth_object
        .entry("refreshToken")
        .or_insert_with(|| Value::String(token.refresh.clone()));
    let accounts = fetch_accounts(&client, &endpoint, &auth, true).await?;
    let account =
        token.extra.get(ACCOUNT_EXTRA).cloned().ok_or_else(|| {
            anyhow!("WorkBuddy credential has no current account; run :login again")
        })?;
    let account = merge_current_account(account, &accounts);
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
    let result = async {
        let response = client
            .get(api_url(endpoint, &format!("/v2{PREFIX_PATH}/accounts")))
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {access}"))
            .header("X-Domain", domain)
            .send()
            .await
            .context("WorkBuddy account list request failed")?;
        let reply = ProviderReply::read(response).await?;
        let payload = reply.payload("WorkBuddy account list")?;
        Ok::<_, anyhow::Error>(
            payload
                .get("accounts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        )
    }
    .await;
    let accounts = match result {
        Ok(accounts) => accounts,
        Err(error) if required => return Err(error),
        Err(_) => Vec::new(),
    };
    if required && accounts.is_empty() {
        bail!("WorkBuddy account list is empty");
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
        // WorkBuddy normally returns expiresAt or expiresIn. Preserve a
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
                    "{label} failed because the WorkBuddy license is unavailable ({code:?}): {message}"
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
        bail!("WorkBuddy login was cancelled");
    }
    tokio::select! {
        result = future => Ok(result?),
        changed = cancel_rx.changed() => {
            let _ = changed;
            bail!("WorkBuddy login was cancelled")
        }
    }
}

async fn wait_to_poll(cancel_rx: &mut watch::Receiver<bool>) -> Result<()> {
    if *cancel_rx.borrow() {
        bail!("WorkBuddy login was cancelled");
    }
    tokio::select! {
        () = tokio::time::sleep(POLL_INTERVAL) => Ok(()),
        changed = cancel_rx.changed() => {
            let _ = changed;
            bail!("WorkBuddy login was cancelled")
        }
    }
}

fn http_client() -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-requested-with",
        "XMLHttpRequest".parse().expect("static header value"),
    );
    headers.insert("x-product", "SaaS".parse().expect("static header value"));
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .build()?)
}

pub(crate) fn default_endpoint(environment: &str) -> Option<&'static str> {
    match environment {
        "internal" => Some(WORKBUDDY_ENDPOINT),
        _ => None,
    }
}

pub(crate) fn normalize_endpoint(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/');
    let url = Url::parse(value).context("invalid WorkBuddy endpoint")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("WorkBuddy endpoint must be an HTTP or HTTPS URL");
    }
    Ok(value.to_string())
}

fn auth_endpoint(value: &str) -> Result<String> {
    let value = normalize_endpoint(value)?;
    Ok(value.strip_suffix("/v2").unwrap_or(&value).to_string())
}

fn endpoint_domain(endpoint: &str) -> Result<String> {
    let url = Url::parse(endpoint).context("invalid WorkBuddy endpoint")?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("WorkBuddy endpoint has no domain"))?;
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
    let mut url = Url::parse(&value).context("invalid WorkBuddy login URL")?;
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
        .ok_or_else(|| anyhow!("WorkBuddy response has no {key}"))
}

fn extra_string(token: &OauthToken, key: &str) -> Option<String> {
    token
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
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
    fn authentication_identity_matches_workbuddy_china() {
        assert_eq!(default_endpoint("internal"), Some(WORKBUDDY_ENDPOINT));
        assert_eq!(default_endpoint("external"), None);
        assert_eq!(default_endpoint("iOA"), None);
        assert_eq!(PLATFORM, "workbuddy");
        assert_eq!(REFERENCE_VERSION, "5.3.14");
        assert_eq!(USER_AGENT, "WorkBuddy/5.3.14 WorkBuddy/5.3.14 CLI/2.115.0");
    }

    #[test]
    fn token_session_keeps_identity_without_duplicating_secrets() {
        let token = token_from_session(
            json!({
                "accessToken": "access",
                "refreshToken": "refresh",
                "expiresIn": 3600,
                "domain": "copilot.tencent.com",
                "method": "github"
            }),
            WORKBUDDY_ENDPOINT,
            "internal",
            json!({"uid": "user-1", "enterpriseId": "enterprise-1"}),
            vec![json!({"uid": "user-1", "enterpriseId": "enterprise-1"})],
        )
        .unwrap();

        assert_eq!(token.access, "access");
        assert_eq!(token.refresh, "refresh");
        assert_eq!(token.extra[DOMAIN_EXTRA], "copilot.tencent.com");
        assert_eq!(token.extra[METHOD_EXTRA], "github");
        assert!(token.extra[AUTH_EXTRA].get("accessToken").is_none());
        assert!(token.extra[AUTH_EXTRA].get("refreshToken").is_none());
    }

    #[test]
    fn provider_reply_unwraps_workbuddy_data_and_pending_codes() {
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
            decorate_auth_url("https://copilot.tencent.com/login?state=one".to_string()).unwrap();
        let url = Url::parse(&url).unwrap();
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "version" && value == REFERENCE_VERSION)
        );
    }

    #[tokio::test]
    async fn login_tolerates_an_unavailable_optional_account_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            let body = json!({"code": 1, "msg": "temporarily unavailable"}).to_string();
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let accounts = fetch_accounts(
            &http_client().unwrap(),
            &format!("http://{address}"),
            &json!({"accessToken": "access", "domain": "copilot.tencent.com"}),
            false,
        )
        .await
        .unwrap();

        assert!(accounts.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_uses_workbuddy_headers_and_preserves_the_current_account() {
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
                            {
                                "uid": "current",
                                "enterpriseId": "one",
                                "lastLogin": false,
                                "departmentFullName": "engineering"
                            },
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
                (
                    ACCOUNT_EXTRA.to_string(),
                    json!({"uid": "current", "enterpriseId": "one"}),
                ),
            ]),
        };

        let refreshed = refresh_token(&token).await.unwrap();
        assert_eq!(refreshed.access, "fresh-access");
        assert_eq!(refreshed.refresh, "rotated-refresh");
        assert_eq!(refreshed.extra[ACCOUNT_EXTRA]["uid"], "current");
        assert_eq!(
            refreshed.extra[ACCOUNT_EXTRA]["departmentFullName"],
            "engineering"
        );
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
        assert!(refresh.contains("x-product: saas"));
        assert!(refresh.contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase())));
        assert!(refresh.ends_with("{}"));
        let accounts = requests[1].to_ascii_lowercase();
        assert!(accounts.starts_with("get /v2/plugin/accounts http/1.1"));
        assert!(accounts.contains("authorization: bearer fresh-access"));
        assert!(accounts.contains("x-domain: enterprise.example"));
        assert!(accounts.contains("x-requested-with: xmlhttprequest"));
        assert!(accounts.contains("x-product: saas"));
        assert!(accounts.contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase())));
    }
}
