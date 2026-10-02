use super::super::util::{http_client, trusted_http_url};
use super::super::{OauthLogin, OauthToken};
use super::shared::{FormUrlEncoded, LoginFlow, device_flow, read_token_form, spawn_login_flow};
use anyhow::{Context, Result};
use serde_json::Value;
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
    let display_host = host.clone();
    spawn_login_flow(
        display_host,
        Some("starting…".to_string()),
        "Open Kimi Code and enter the device code.",
        move |flow| kimi_login(host, flow),
    )
}

async fn kimi_login(host: String, flow: LoginFlow) -> Result<OauthToken> {
    let client = http_client()?;
    let token_client = client.clone();
    let token_url = format!("{host}/api/oauth/token");
    device_flow(
        flow.cancel_rx,
        flow.display,
        "Open Kimi Code and enter the device code.",
        "Kimi Code",
        client
            .post(format!("{host}/api/oauth/device_authorization"))
            .header("Accept", "application/json")
            .form_urlencoded(&[("client_id", KIMI_CLIENT_ID)]),
        |value| {
            let verification = value
                .get("verification_uri_complete")
                .and_then(Value::as_str)
                .or_else(|| value.get("verification_uri").and_then(Value::as_str))
                .unwrap_or_default();
            trusted_http_url(verification)
        },
        true,
        Some(Duration::from_secs(15 * 60)),
        move |device_code| {
            token_client
                .post(token_url.clone())
                .header("Accept", "application/json")
                .form_urlencoded(&[
                    ("client_id", KIMI_CLIENT_ID),
                    ("device_code", device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
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
    use super::super::shared::test_http::read_request;
    use super::*;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

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
