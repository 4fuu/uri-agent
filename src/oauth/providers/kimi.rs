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
    refresh_kimi_at(&host(), refresh).await
}

async fn refresh_kimi_at(host: &str, refresh: &str) -> Result<OauthToken> {
    let client = http_client()?;
    for attempt in 0..=3 {
        let response = client
            .post(format!("{host}/api/oauth/token"))
            .form_urlencoded(&[
                ("client_id", KIMI_CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
            ])
            .send()
            .await;
        match response {
            Ok(response) if kimi_refresh_retryable(response.status()) && attempt < 3 => {}
            Ok(response) => return read_token_form(response, "Kimi Code").await,
            Err(_) if attempt < 3 => {}
            Err(error) => return Err(error).context("Kimi Code token refresh failed"),
        }
        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
    }
    unreachable!("Kimi refresh loop returns after its final attempt")
}

fn kimi_refresh_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
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
    fn refresh_retries_only_transient_statuses() {
        assert!(kimi_refresh_retryable(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(kimi_refresh_retryable(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(!kimi_refresh_retryable(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!kimi_refresh_retryable(reqwest::StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn refresh_retries_transient_failure_and_accepts_rotation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in [
                (500, r#"{"error":"temporary"}"#),
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

        let token = refresh_kimi_at(&format!("http://{address}"), "old-refresh")
            .await
            .unwrap();
        assert_eq!(token.access, "fresh-access");
        assert_eq!(token.refresh, "rotated-refresh");
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.contains("refresh_token=old-refresh"))
        );
    }
}
