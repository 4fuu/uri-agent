mod requests;

use super::*;

pub(super) struct ParallelSearchOptions {
    pub(super) mode: String,
    search_queries: Vec<String>,
    pub(super) location: Option<String>,
    pub(super) include_domains: Vec<String>,
    pub(super) exclude_domains: Vec<String>,
    after_date: Option<String>,
    max_chars_total: Option<u64>,
    pub(super) max_chars_per_result: u64,
    max_age_seconds: Option<u64>,
    timeout_seconds: Option<f64>,
    disable_cache_fallback: Option<bool>,
    session_id: Option<String>,
    client_model: Option<String>,
}

impl ParallelSearchOptions {
    pub(super) fn parse(query: &str, pairs: &[(String, String)]) -> Result<(usize, Self)> {
        let mut limit = None;
        let mut mode = None;
        let mut search_queries = Vec::new();
        let mut location = None;
        let mut include_domains = Vec::new();
        let mut exclude_domains = Vec::new();
        let mut after_date = None;
        let mut max_chars_total = None;
        let mut max_chars_per_result = None;
        let mut max_age_seconds = None;
        let mut timeout_seconds = None;
        let mut disable_cache_fallback = None;
        let mut session_id = None;
        let mut client_model = None;

        for (name, value) in pairs {
            match name.as_str() {
                "limit" => {
                    ensure_once(&limit, name)?;
                    limit = Some(parse_search_limit(value)?);
                }
                "mode" => {
                    ensure_once(&mode, name)?;
                    require_choice(value, name, &["turbo", "fast", "basic", "advanced"])?;
                    mode = Some(value.clone());
                }
                "search_query" => {
                    let value = require_text(value, name)?;
                    if value.chars().count() > 200 {
                        bail!("https://search search_query must not exceed 200 characters");
                    }
                    search_queries.push(value.to_string());
                }
                "location" => {
                    ensure_once(&location, name)?;
                    let value = parse_location(value)?;
                    location = Some(value);
                }
                "include_domain" => {
                    validate_parallel_domain(value, name)?;
                    include_domains.push(value.clone());
                }
                "exclude_domain" => {
                    validate_parallel_domain(value, name)?;
                    exclude_domains.push(value.clone());
                }
                "after_date" => {
                    ensure_once(&after_date, name)?;
                    validate_date(value, name)?;
                    after_date = Some(value.clone());
                }
                "max_chars_total" => {
                    ensure_once(&max_chars_total, name)?;
                    max_chars_total = Some(parse_positive_u64(value, name)?);
                }
                "max_chars_per_result" => {
                    ensure_once(&max_chars_per_result, name)?;
                    max_chars_per_result = Some(parse_positive_u64(value, name)?);
                }
                "max_age_seconds" => {
                    ensure_once(&max_age_seconds, name)?;
                    let age = value.parse::<u64>().map_err(|_| {
                        anyhow!("https://search max_age_seconds must be an integer")
                    })?;
                    if age < 600 {
                        bail!("https://search max_age_seconds must be at least 600");
                    }
                    max_age_seconds = Some(age);
                }
                "timeout_seconds" => {
                    ensure_once(&timeout_seconds, name)?;
                    let timeout = value
                        .parse::<f64>()
                        .map_err(|_| anyhow!("https://search timeout_seconds must be a number"))?;
                    if !timeout.is_finite() || timeout <= 0.0 {
                        bail!("https://search timeout_seconds must be positive");
                    }
                    timeout_seconds = Some(timeout);
                }
                "disable_cache_fallback" => {
                    ensure_once(&disable_cache_fallback, name)?;
                    disable_cache_fallback = Some(parse_bool(value, name)?);
                }
                "session_id" => {
                    ensure_once(&session_id, name)?;
                    let value = require_text(value, name)?;
                    if value.chars().count() > 1_000 {
                        bail!("https://search session_id must not exceed 1000 characters");
                    }
                    session_id = Some(value.to_string());
                }
                "client_model" => {
                    ensure_once(&client_model, name)?;
                    client_model = Some(require_text(value, name)?.to_string());
                }
                _ => bail!(
                    "https://search option is not supported by Parallel: {name}; read https://help/parallel"
                ),
            }
        }

        if query.chars().count() > 5_000 {
            bail!("Parallel search objective must not exceed 5000 characters");
        }
        if search_queries.len() > 5 {
            bail!("https://search accepts at most 5 Parallel search_query options");
        }
        if search_queries.is_empty() {
            if query.chars().count() > 200 {
                bail!(
                    "Parallel search bodies over 200 characters require at least one search_query option"
                );
            }
            search_queries.push(query.to_string());
        }
        if include_domains.len() + exclude_domains.len() > 200 {
            bail!("Parallel search accepts at most 200 domain options");
        }

        Ok((
            limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
            Self {
                mode: mode.unwrap_or_else(|| "advanced".to_string()),
                search_queries,
                location,
                include_domains,
                exclude_domains,
                after_date,
                max_chars_total,
                max_chars_per_result: max_chars_per_result.unwrap_or(DEFAULT_SNIPPET_CHARS as u64),
                max_age_seconds,
                timeout_seconds,
                disable_cache_fallback,
                session_id,
                client_model,
            },
        ))
    }
}

#[derive(Deserialize)]
struct ParallelSearchResponse {
    #[serde(default)]
    search_id: Option<String>,
    #[serde(default)]
    results: Vec<ParallelSearchResult>,
    #[serde(default)]
    warnings: Option<Vec<ProviderWarning>>,
}

#[derive(Deserialize)]
struct ProviderWarning {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct ParallelSearchResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    publish_date: Option<String>,
    excerpts: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ParallelExtractResponse {
    #[serde(default)]
    results: Vec<ParallelExtractResult>,
    #[serde(default)]
    errors: Vec<ParallelExtractError>,
}

#[derive(Deserialize)]
struct ParallelExtractResult {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    publish_date: Option<String>,
    #[serde(default)]
    excerpts: Vec<String>,
    #[serde(default)]
    full_content: Option<String>,
}

#[derive(Deserialize)]
struct ParallelExtractError {
    #[serde(default)]
    error_type: String,
    #[serde(default)]
    http_status_code: Option<u16>,
    #[serde(default)]
    content: Option<String>,
}

impl ParallelExtractError {
    fn describe(self) -> String {
        let mut detail = if self.error_type.trim().is_empty() {
            "extract error".to_string()
        } else {
            self.error_type
        };
        if let Some(status) = self.http_status_code {
            let _ = write!(detail, " (HTTP {status})");
        }
        if let Some(content) = self.content.and_then(nonempty) {
            let _ = write!(detail, ": {}", single_line(&content));
        }
        detail
    }
}
