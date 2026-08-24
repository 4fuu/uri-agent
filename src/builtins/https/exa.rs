mod requests;

use super::*;

#[derive(Clone, Copy)]
enum ExaContent {
    Highlights,
    Text,
    Summary,
}

impl ExaContent {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "highlights" => Ok(Self::Highlights),
            "text" => Ok(Self::Text),
            "summary" => Ok(Self::Summary),
            _ => bail!("https://search content must be highlights, text, or summary"),
        }
    }

    fn api_field(self) -> &'static str {
        match self {
            Self::Highlights => "highlights",
            Self::Text => "text",
            Self::Summary => "summary",
        }
    }
}

pub(super) struct ExaSearchOptions {
    search_type: String,
    category: Option<String>,
    location: Option<String>,
    include_domains: Vec<String>,
    exclude_domains: Vec<String>,
    start_published_date: Option<String>,
    end_published_date: Option<String>,
    moderation: Option<bool>,
    content: ExaContent,
    pub(super) max_characters: Option<u64>,
    max_age_hours: Option<i64>,
    livecrawl_timeout: Option<u64>,
    additional_queries: Vec<String>,
    subpages: Option<usize>,
    subpage_targets: Vec<String>,
    system_prompt: Option<String>,
}

impl ExaSearchOptions {
    pub(super) fn parse(pairs: &[(String, String)]) -> Result<(usize, Self)> {
        let mut limit = None;
        let mut search_type = None;
        let mut category = None;
        let mut location = None;
        let mut include_domains = Vec::new();
        let mut exclude_domains = Vec::new();
        let mut start_published_date = None;
        let mut end_published_date = None;
        let mut moderation = None;
        let mut content = None;
        let mut max_characters = None;
        let mut max_age_hours = None;
        let mut livecrawl_timeout = None;
        let mut additional_queries = Vec::new();
        let mut subpages = None;
        let mut subpage_targets = Vec::new();
        let mut system_prompt = None;

        for (name, value) in pairs {
            match name.as_str() {
                "limit" => {
                    ensure_once(&limit, name)?;
                    limit = Some(parse_search_limit(value)?);
                }
                "type" => {
                    ensure_once(&search_type, name)?;
                    require_choice(
                        value,
                        name,
                        &[
                            "instant",
                            "fast",
                            "auto",
                            "deep-lite",
                            "deep",
                            "deep-reasoning",
                        ],
                    )?;
                    search_type = Some(value.clone());
                }
                "category" => {
                    ensure_once(&category, name)?;
                    require_choice(
                        value,
                        name,
                        &[
                            "company",
                            "people",
                            "publication",
                            "news",
                            "personal site",
                            "financial report",
                        ],
                    )?;
                    category = Some(value.clone());
                }
                "location" => {
                    ensure_once(&location, name)?;
                    location = Some(parse_location(value)?.to_ascii_uppercase());
                }
                "include_domain" => {
                    validate_exa_domain(value, name)?;
                    include_domains.push(value.clone());
                }
                "exclude_domain" => {
                    validate_exa_domain(value, name)?;
                    exclude_domains.push(value.clone());
                }
                "start_published_date" => {
                    ensure_once(&start_published_date, name)?;
                    validate_iso_date_or_datetime(value, name)?;
                    start_published_date = Some(value.clone());
                }
                "end_published_date" => {
                    ensure_once(&end_published_date, name)?;
                    validate_iso_date_or_datetime(value, name)?;
                    end_published_date = Some(value.clone());
                }
                "moderation" => {
                    ensure_once(&moderation, name)?;
                    moderation = Some(parse_bool(value, name)?);
                }
                "content" => {
                    ensure_once(&content, name)?;
                    content = Some(ExaContent::parse(value)?);
                }
                "max_characters" => {
                    ensure_once(&max_characters, name)?;
                    let limit = parse_positive_u64(value, name)?;
                    if limit > EXA_MAX_CONTENT_CHARS {
                        bail!(
                            "https://search max_characters must not exceed {EXA_MAX_CONTENT_CHARS} for Exa"
                        );
                    }
                    max_characters = Some(limit);
                }
                "max_age_hours" => {
                    ensure_once(&max_age_hours, name)?;
                    let age = value
                        .parse::<i64>()
                        .map_err(|_| anyhow!("https://search max_age_hours must be an integer"))?;
                    if !(-1..=EXA_MAX_AGE_HOURS).contains(&age) {
                        bail!(
                            "https://search max_age_hours must be between -1 and {EXA_MAX_AGE_HOURS} for Exa"
                        );
                    }
                    max_age_hours = Some(age);
                }
                "livecrawl_timeout" => {
                    ensure_once(&livecrawl_timeout, name)?;
                    let timeout = parse_positive_u64(value, name)?;
                    if timeout > EXA_MAX_LIVECRAWL_TIMEOUT {
                        bail!(
                            "https://search livecrawl_timeout must not exceed {EXA_MAX_LIVECRAWL_TIMEOUT} for Exa"
                        );
                    }
                    livecrawl_timeout = Some(timeout);
                }
                "additional_query" => {
                    additional_queries.push(require_text(value, name)?.to_string());
                }
                "subpages" => {
                    ensure_once(&subpages, name)?;
                    let count = value.parse::<usize>().map_err(|_| {
                        anyhow!("https://search subpages must be a nonnegative integer")
                    })?;
                    if count > EXA_MAX_SUBPAGES {
                        bail!("https://search subpages must not exceed {EXA_MAX_SUBPAGES} for Exa");
                    }
                    subpages = Some(count);
                }
                "subpage_target" => {
                    let target = require_text(value, name)?;
                    if target.chars().count() > 100 {
                        bail!(
                            "https://search subpage_target must not exceed 100 characters for Exa"
                        );
                    }
                    subpage_targets.push(target.to_string());
                }
                "system_prompt" => {
                    ensure_once(&system_prompt, name)?;
                    system_prompt = Some(require_text(value, name)?.to_string());
                }
                _ => bail!(
                    "https://search option is not supported by Exa: {name}; read https://help/exa"
                ),
            }
        }

        if include_domains.len() > 1_200 || exclude_domains.len() > 1_200 {
            bail!("Exa search accepts at most 1200 include or exclude domains");
        }
        if additional_queries.len() > 10 {
            bail!("Exa search accepts at most 10 additional_query options");
        }
        if subpage_targets.len() > 100 {
            bail!("Exa search accepts at most 100 subpage_target options");
        }
        let search_type = search_type.unwrap_or_else(|| "auto".to_string());
        if !additional_queries.is_empty() && !search_type.starts_with("deep") {
            bail!("Exa additional_query requires a deep search type");
        }
        if !subpage_targets.is_empty() && subpages.unwrap_or(0) == 0 {
            bail!("Exa subpage_target requires a positive subpages option");
        }
        if matches!(category.as_deref(), Some("company" | "people"))
            && (!exclude_domains.is_empty()
                || start_published_date.is_some()
                || end_published_date.is_some())
        {
            bail!(
                "Exa company and people categories do not support exclude_domain or publication dates"
            );
        }
        let content = content.unwrap_or(ExaContent::Highlights);
        if matches!(content, ExaContent::Summary) && max_characters.is_some() {
            bail!("Exa max_characters does not apply to summary content");
        }

        Ok((
            limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
            Self {
                search_type,
                category,
                location,
                include_domains,
                exclude_domains,
                start_published_date,
                end_published_date,
                moderation,
                content,
                max_characters,
                max_age_hours,
                livecrawl_timeout,
                additional_queries,
                subpages,
                subpage_targets,
                system_prompt,
            },
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaSearchResponse {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    results: Vec<ExaSearchResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaSearchResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
    #[serde(default)]
    text: Option<String>,
    highlights: Option<Vec<String>>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    subpages: Vec<ExaSubpage>,
}

#[derive(Deserialize)]
struct ExaSubpage {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaContentsResponse {
    #[serde(default)]
    results: Vec<ExaContentsResult>,
    #[serde(default)]
    statuses: Vec<ExaContentsStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaContentsResult {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct ExaContentsStatus {
    #[serde(default)]
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<ExaContentsError>,
}

impl ExaContentsStatus {
    fn describe(self) -> String {
        let mut detail = format!("{}: {}", self.id, self.status);
        if let Some(error) = self.error {
            if let Some(tag) = nonempty(error.tag) {
                let _ = write!(detail, " ({tag})");
            }
            if let Some(status) = error.http_status_code {
                let _ = write!(detail, " (HTTP {status})");
            }
        }
        detail
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExaContentsError {
    #[serde(default)]
    tag: String,
    #[serde(default)]
    http_status_code: Option<u16>,
}
