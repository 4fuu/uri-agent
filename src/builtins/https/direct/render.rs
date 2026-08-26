use super::super::*;

pub(super) async fn render_page(
    final_url: &Url,
    content_type: &str,
    body: Vec<u8>,
) -> Result<Vec<u8>> {
    let text = String::from_utf8(body).context("HTTPS response is not valid UTF-8 text")?;
    let html = is_html(content_type, &text);
    let content = if html {
        html_to_markdown(text).await?
    } else if content_type.contains("json") {
        serde_json::from_str::<Value>(&text)
            .and_then(|value| serde_json::to_string_pretty(&value))
            .unwrap_or(text)
    } else if is_textual(content_type) || content_type.is_empty() {
        text
    } else {
        bail!("unsupported HTTPS response content type: {content_type}");
    };
    let mut output = format!("{UNTRUSTED_WEB_CONTENT}\nSource: {final_url}\n");
    if !html && !content_type.is_empty() {
        output.push_str(&format!("Content-Type: {content_type}\n"));
    }
    output.push('\n');
    output.push_str(content.trim());
    output.push('\n');
    Ok(output.into_bytes())
}

async fn html_to_markdown(html: String) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let options = ConversionOptions {
            preprocessing: PreprocessingOptions {
                enabled: true,
                preset: PreprocessingPreset::Aggressive,
                remove_navigation: true,
                remove_forms: true,
            },
            tier_strategy: TierStrategy::Tier2,
            ..Default::default()
        };
        let result = convert(&html, Some(options)).context("HTML to Markdown conversion failed")?;
        if let Some(warning) = result
            .warnings
            .iter()
            .find(|warning| warning.kind == WarningKind::DepthLimitExceeded)
        {
            bail!("HTML to Markdown conversion failed: {}", warning.message);
        }
        Ok(result.content.unwrap_or_default())
    })
    .await
    .context("HTML conversion worker failed")?
}

pub(super) fn response_content_type(response: &Response) -> String {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn is_html(content_type: &str, content: &str) -> bool {
    if content_type.contains("html") || content_type.contains("xhtml") {
        return true;
    }
    let prefix = content
        .chars()
        .take(512)
        .collect::<String>()
        .to_ascii_lowercase();
    ["<!doctype html", "<html", "<head", "<body"]
        .iter()
        .any(|marker| prefix.contains(marker))
}

fn is_textual(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.contains("xml")
        || content_type.contains("javascript")
}
