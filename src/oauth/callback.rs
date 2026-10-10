use super::util::{form_pairs, parse_authorization_input};
use anyhow::{Context, Result, anyhow, bail};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

pub struct CallbackListener {
    listener: Option<TcpListener>,
    pub redirect_uri: String,
    expected_path: String,
    expected_state: Option<String>,
    success: String,
}

pub async fn bind(
    host: &str,
    port: u16,
    path: &str,
    public_redirect: &str,
    expected_state: Option<&str>,
    success: &str,
) -> CallbackListener {
    let listener = TcpListener::bind((host, port)).await.ok();
    CallbackListener {
        listener,
        redirect_uri: public_redirect.to_string(),
        expected_path: path.to_string(),
        expected_state: expected_state.map(str::to_string),
        success: success.to_string(),
    }
}

/// Bind the preferred fixed port, or let the OS assign a free loopback port
/// when it is excluded or already taken. The redirect URI always names the
/// port actually bound, so the authorize URL and the token exchange agree.
/// Use only for authorization servers that accept any loopback port (RFC
/// 8252 §7.3); protocol-pinned ports must keep [`bind`].
pub async fn bind_preferred(
    host: &str,
    preferred_port: u16,
    path: &str,
    public_host: &str,
    expected_state: Option<&str>,
    success: &str,
) -> CallbackListener {
    let listener = match TcpListener::bind((host, preferred_port)).await {
        Ok(listener) => Some(listener),
        Err(_) => TcpListener::bind((host, 0)).await.ok(),
    };
    // `None` only when even the OS-assigned bind failed, which is practically
    // impossible on loopback; the paste-only fallback then stays alive.
    let port = listener
        .as_ref()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
        .unwrap_or(preferred_port);
    CallbackListener {
        listener,
        redirect_uri: format!("http://{public_host}:{port}{path}"),
        expected_path: path.to_string(),
        expected_state: expected_state.map(str::to_string),
        success: success.to_string(),
    }
}

pub async fn bind_ephemeral(
    host: &str,
    path: &str,
    expected_state: Option<&str>,
    success: &str,
) -> Result<CallbackListener> {
    let listener = TcpListener::bind((host, 0)).await?;
    let port = listener.local_addr()?.port();
    Ok(CallbackListener {
        listener: Some(listener),
        redirect_uri: format!("http://{host}:{port}{path}"),
        expected_path: path.to_string(),
        expected_state: expected_state.map(str::to_string),
        success: success.to_string(),
    })
}

impl CallbackListener {
    pub async fn accept_code(&self) -> Result<Option<(String, Option<String>)>> {
        let Some(listener) = &self.listener else {
            std::future::pending::<()>().await;
            return Ok(None);
        };
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
                .await
                .context("OAuth callback timed out")??;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if buffer.len() > 16 * 1024 {
                bail!("OAuth callback request is too large");
            }
        }
        let request = String::from_utf8_lossy(&buffer);
        let first_line = request.lines().next().unwrap_or_default();
        let target = first_line.split_whitespace().nth(1).unwrap_or_default();
        let uri = http::Uri::try_from(target).unwrap_or_else(|_| http::Uri::from_static("/"));
        if uri.path() != self.expected_path {
            let _ = stream
                .write_all(http_page(404, &error_page("Callback route not found.")).as_bytes())
                .await;
            return Ok(None);
        }
        let query = uri.query().unwrap_or_default();
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (key, value) in form_pairs(query) {
            match key.as_str() {
                "code" => code = Some(value),
                "state" => state = Some(value),
                "error" => error = Some(value),
                _ => {}
            }
        }
        if let Some(error) = error {
            let _ = stream
                .write_all(
                    http_page(
                        400,
                        &error_page(&format!("Authentication did not complete: {error}")),
                    )
                    .as_bytes(),
                )
                .await;
            bail!("OAuth authorization failed: {error}");
        }
        let Some(code) = code else {
            let _ = stream
                .write_all(http_page(400, &error_page("Missing authorization code.")).as_bytes())
                .await;
            return Ok(None);
        };
        if let Some(expected) = &self.expected_state
            && state.as_deref() != Some(expected.as_str())
        {
            let _ = stream
                .write_all(http_page(400, &error_page("OAuth state mismatch.")).as_bytes())
                .await;
            bail!("OAuth state mismatch");
        }
        let _ = stream
            .write_all(http_page(200, &success_page(&self.success)).as_bytes())
            .await;
        Ok(Some((code, state)))
    }
}

pub async fn race_callback_or_paste(
    callback: &CallbackListener,
    paste_rx: &mut mpsc::Receiver<String>,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    expected_state: Option<&str>,
) -> Result<(String, Option<String>)> {
    loop {
        tokio::select! {
            changed = cancel_rx.changed() => {
                if changed.is_err() || *cancel_rx.borrow() {
                    bail!("OAuth login was cancelled");
                }
            }
            paste = paste_rx.recv() => {
                let Some(paste) = paste else {
                    bail!("OAuth login was cancelled");
                };
                let (code, state) = parse_authorization_input(&paste)
                    .ok_or_else(|| anyhow!("paste a redirect URL or authorization code"))?;
                if let (Some(expected), Some(state)) = (expected_state, state.as_deref())
                    && expected != state
                {
                    bail!("OAuth state mismatch");
                }
                return Ok((code, state));
            }
            accepted = callback.accept_code() => {
                match accepted {
                    Ok(Some(result)) => return Ok(result),
                    Ok(None) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

fn http_page(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn success_page(message: &str) -> String {
    oauth_page("Authentication successful", message)
}

fn error_page(message: &str) -> String {
    oauth_page("Authentication failed", message)
}

fn oauth_page(heading: &str, message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"/><title>{heading}</title>
<style>body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:#0d0f12;color:#dadfe5;font-family:sans-serif;text-align:center}}main{{max-width:28rem;padding:2rem}}h1{{color:#68d2c2}}</style></head>
<body><main><h1>{}</h1><p>{}</p></main></body></html>",
        escape_html(heading),
        escape_html(message)
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_preferred_uses_the_fixed_port_when_available() {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let callback = bind_preferred("127.0.0.1", port, "/cb", "localhost", None, "done").await;
        assert_eq!(callback.redirect_uri, format!("http://localhost:{port}/cb"));
        assert!(callback.listener.is_some());
    }

    #[tokio::test]
    async fn bind_preferred_falls_back_to_an_os_assigned_port() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let callback = bind_preferred("127.0.0.1", port, "/cb", "localhost", None, "done").await;
        assert_ne!(callback.redirect_uri, format!("http://localhost:{port}/cb"));
        assert!(callback.redirect_uri.starts_with("http://localhost:"));
        assert!(callback.redirect_uri.ends_with("/cb"));
        assert!(callback.listener.is_some());
    }
}
