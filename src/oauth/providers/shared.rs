use super::super::OauthToken;
use super::super::device::{self, Poll};
use super::super::util::{form_body, open_url};
use super::super::{LoginSetup, OauthDisplay, OauthLogin, channels, set_display};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, watch};

pub(super) async fn read_token_form(
    response: reqwest::Response,
    label: &str,
) -> Result<OauthToken> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{label} token request failed ({status}): {value}");
    }
    token_from_value(&value)
}

pub(super) async fn json_or_error(response: reqwest::Response, label: &str) -> Result<Value> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{label} failed ({status}): {value}");
    }
    Ok(value)
}

pub(super) async fn oauth_poll_from_token_response(
    response: reqwest::Response,
    label: &str,
) -> Result<Poll<OauthToken>> {
    let status = response.status();
    let value = response.json::<Value>().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(Poll::Complete(token_from_value(&value)?));
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

/// The interactive half of a login flow: what the spawned flow task uses to
/// read pasted input, observe cancellation, and update the display.
pub(super) struct LoginFlow {
    pub(super) paste_rx: tokio::sync::mpsc::Receiver<String>,
    pub(super) cancel_rx: watch::Receiver<bool>,
    pub(super) display: Arc<Mutex<OauthDisplay>>,
}

/// Start one OAuth login: build the display and channels, run `run` on a
/// background task, and report its outcome through the returned receiver.
/// Every provider login is spawned exactly this way, so cancellation,
/// display updates, and the completion channel cannot drift apart.
pub(super) fn spawn_login_flow<F, Fut>(
    url: String,
    user_code: Option<String>,
    instructions: &str,
    run: F,
) -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)>
where
    F: FnOnce(LoginFlow) -> Fut + Send + 'static,
    Fut: Future<Output = Result<OauthToken>> + Send + 'static,
{
    let LoginSetup {
        login,
        paste_rx,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    } = channels(url, user_code, instructions);
    tokio::spawn(async move {
        let result = run(LoginFlow {
            paste_rx,
            cancel_rx,
            display,
        })
        .await;
        let _ = done_tx.send(result);
    });
    Ok((login, done_rx))
}

/// Drive one OAuth 2.0 device-authorization flow end to end: request a
/// device code, show the verification URL, then poll the token endpoint
/// until the login completes, is denied, or times out.
///
/// `device_request` is the provider's device-authorization POST;
/// `verification` extracts and validates the user-facing verification URL
/// from the authorization response; `token_request` builds the token POST
/// for each poll from the device code. `provider_label` names the provider
/// in errors, for example "xAI". `fallback_expires` is the polling deadline
/// used when the response carries no usable `expires_in`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn device_flow(
    cancel_rx: watch::Receiver<bool>,
    display: Arc<Mutex<OauthDisplay>>,
    instructions: &str,
    provider_label: &str,
    device_request: reqwest::RequestBuilder,
    verification: impl FnOnce(&Value) -> Result<String>,
    wait_before_first_poll: bool,
    fallback_expires: Option<Duration>,
    token_request: impl Fn(&str) -> reqwest::RequestBuilder + Clone + Send,
) -> Result<OauthToken> {
    let authorization_label = format!("{provider_label} device authorization");
    let value = device_request
        .send()
        .await
        .with_context(|| format!("{authorization_label} failed"))?;
    let value = json_or_error(value, &authorization_label).await?;
    let device_code = required_str(&value, "device_code")?;
    let user_code = required_str(&value, "user_code")?;
    let verification = verification(&value)?;
    set_display(
        &display,
        verification.clone(),
        Some(user_code),
        instructions,
    );
    open_url(&verification);
    device::poll(
        json_interval(&value),
        json_expires(&value).or(fallback_expires),
        wait_before_first_poll,
        cancel_rx,
        move || {
            let token_request = token_request.clone();
            let device_code = device_code.clone();
            async move {
                let response = token_request(&device_code).send().await?;
                oauth_poll_from_token_response(response, provider_label).await
            }
        },
    )
    .await
}

pub(super) fn token_from_value(value: &Value) -> Result<OauthToken> {
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

pub(super) fn required_str(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing {field}"))
}

pub(super) fn json_interval(value: &Value) -> Option<Duration> {
    value
        .get("interval")
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|value| *value > 0.0)
        .map(Duration::from_secs_f64)
}

pub(super) fn json_expires(value: &Value) -> Option<Duration> {
    value
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
}

pub(super) trait FormUrlEncoded {
    fn form_urlencoded(self, fields: &[(&str, &str)]) -> Self;
}

impl FormUrlEncoded for reqwest::RequestBuilder {
    fn form_urlencoded(self, fields: &[(&str, &str)]) -> Self {
        self.header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body(fields))
    }
}

pub(super) fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|error| anyhow!("cannot generate OAuth state: {error}"))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Read one full HTTP request (headers plus content-length body) from a mock
/// server socket. Shared by the provider flow tests that assert request
/// headers and bodies against a local listener.
#[cfg(test)]
pub(crate) mod test_http {
    use tokio::io::AsyncReadExt as _;
    use tokio::net::TcpStream;

    pub(crate) async fn read_request(socket: &mut TcpStream) -> String {
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
}
