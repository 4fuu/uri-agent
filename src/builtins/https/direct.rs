mod render;

use super::*;
use render::{render_page, response_content_type};

impl HttpsProtocol {
    pub(super) async fn fetch_page(&self, url: Url) -> Result<Vec<u8>> {
        let response = self
            .page_client
            .get(url.clone())
            .header(
                reqwest::header::ACCEPT,
                "text/markdown, text/html;q=0.9, application/json;q=0.8, text/plain;q=0.7, */*;q=0.1",
            )
            .send()
            .await
            .with_context(|| format!("HTTPS request failed: {url}"))?;
        let status = response.status();
        let final_url = response.url().clone();
        let content_type = response_content_type(&response);
        if !status.is_success() {
            let body = read_response_prefix(response, MAX_ERROR_BYTES).await?;
            return Err(http_status_error("HTTPS request", status, &body));
        }
        let body = read_response(response, MAX_RESPONSE_BYTES).await?;
        render_page(&final_url, &content_type, body).await
    }
}

pub(super) async fn read_response(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > limit {
            bail!("HTTPS response exceeded the {limit}-byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(super) async fn read_response_prefix(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while body.len() < limit {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let remaining = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(body)
}

pub(super) fn http_status_error(label: &str, status: StatusCode, body: &[u8]) -> anyhow::Error {
    let detail = single_line(&String::from_utf8_lossy(body));
    if detail.is_empty() {
        anyhow!("{label} failed with HTTP {status}")
    } else {
        anyhow!("{label} failed with HTTP {status}: {detail}")
    }
}
