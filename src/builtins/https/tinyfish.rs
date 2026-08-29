mod requests;

use super::*;

const TINYFISH_PAGE_SIZE: usize = 10;
const TINYFISH_MAX_PAGE: u32 = 10;
const TINYFISH_MAX_RECENCY_MINUTES: u64 = 5_256_000;
const TINYFISH_MAX_PURPOSE_CHARS: usize = 2_000;

pub(super) struct TinyfishSearchOptions {
    pub(super) location: Option<String>,
    pub(super) language: Option<String>,
    pub(super) include_domains: Vec<String>,
    pub(super) exclude_domains: Vec<String>,
    pub(super) recency_minutes: Option<u64>,
    pub(super) after_date: Option<String>,
    pub(super) before_date: Option<String>,
    pub(super) domain_type: String,
    pub(super) pub_year_min: Option<u64>,
    pub(super) pub_year_max: Option<u64>,
    pub(super) purpose: Option<String>,
}

impl TinyfishSearchOptions {
    pub(super) fn parse(pairs: &[(String, String)]) -> Result<(usize, Self)> {
        let mut limit = None;
        let mut location = None;
        let mut language = None;
        let mut include_domains = Vec::new();
        let mut exclude_domains = Vec::new();
        let mut recency_minutes = None;
        let mut after_date = None;
        let mut before_date = None;
        let mut domain_type = None;
        let mut pub_year_min = None;
        let mut pub_year_max = None;
        let mut purpose = None;

        for (name, value) in pairs {
            match name.as_str() {
                "limit" => {
                    ensure_once(&limit, name)?;
                    limit = Some(parse_search_limit(value)?);
                }
                "location" => {
                    ensure_once(&location, name)?;
                    location = Some(parse_location(value)?.to_ascii_uppercase());
                }
                "language" => {
                    ensure_once(&language, name)?;
                    language = Some(parse_language(value)?);
                }
                "include_domain" => {
                    validate_domain(value, name)?;
                    include_domains.push(value.clone());
                }
                "exclude_domain" => {
                    validate_domain(value, name)?;
                    exclude_domains.push(value.clone());
                }
                "recency_minutes" => {
                    ensure_once(&recency_minutes, name)?;
                    let minutes = value.parse::<u64>().map_err(|_| {
                        anyhow!("https://search recency_minutes must be an integer")
                    })?;
                    if !(1..=TINYFISH_MAX_RECENCY_MINUTES).contains(&minutes) {
                        bail!(
                            "https://search recency_minutes must be between 1 and {TINYFISH_MAX_RECENCY_MINUTES}"
                        );
                    }
                    recency_minutes = Some(minutes);
                }
                "after_date" => {
                    ensure_once(&after_date, name)?;
                    validate_date(value, name)?;
                    after_date = Some(value.clone());
                }
                "before_date" => {
                    ensure_once(&before_date, name)?;
                    validate_date(value, name)?;
                    before_date = Some(value.clone());
                }
                "domain_type" => {
                    ensure_once(&domain_type, name)?;
                    require_choice(value, name, &["web", "news", "research_paper"])?;
                    domain_type = Some(value.clone());
                }
                "pub_year_min" => {
                    ensure_once(&pub_year_min, name)?;
                    pub_year_min = Some(parse_publication_year(value, name)?);
                }
                "pub_year_max" => {
                    ensure_once(&pub_year_max, name)?;
                    pub_year_max = Some(parse_publication_year(value, name)?);
                }
                "purpose" => {
                    ensure_once(&purpose, name)?;
                    let value = require_text(value, name)?;
                    if value.chars().count() > TINYFISH_MAX_PURPOSE_CHARS {
                        bail!(
                            "https://search purpose must not exceed {TINYFISH_MAX_PURPOSE_CHARS} characters"
                        );
                    }
                    purpose = Some(value.to_string());
                }
                _ => bail!(
                    "https://search option is not supported by TinyFish: {name}; read https://help/tinyfish"
                ),
            }
        }

        if recency_minutes.is_some() && (after_date.is_some() || before_date.is_some()) {
            bail!("TinyFish recency_minutes cannot be combined with after_date or before_date");
        }
        if let (Some(after), Some(before)) = (&after_date, &before_date)
            && after > before
        {
            bail!("TinyFish after_date must not be later than before_date");
        }
        let domain_type = domain_type.unwrap_or_else(|| "web".to_string());
        if domain_type == "research_paper" {
            if recency_minutes.is_some() || after_date.is_some() || before_date.is_some() {
                bail!(
                    "TinyFish research_paper search does not support recency_minutes, after_date, or before_date; use pub_year_min and pub_year_max"
                );
            }
        } else if pub_year_min.is_some() || pub_year_max.is_some() {
            bail!("TinyFish pub_year_min and pub_year_max require domain_type=research_paper");
        }
        if let (Some(min), Some(max)) = (pub_year_min, pub_year_max)
            && min > max
        {
            bail!("TinyFish pub_year_min must not exceed pub_year_max");
        }

        Ok((
            limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
            Self {
                location,
                language,
                include_domains,
                exclude_domains,
                recency_minutes,
                after_date,
                before_date,
                domain_type,
                pub_year_min,
                pub_year_max,
                purpose,
            },
        ))
    }
}

fn parse_language(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_lowercase()) {
        bail!("https://search language must be a two-letter language code");
    }
    Ok(value)
}

fn parse_publication_year(value: &str, name: &str) -> Result<u64> {
    let year = value
        .parse::<u64>()
        .map_err(|_| anyhow!("https://search {name} must be an integer"))?;
    if year > 9_999 {
        bail!("https://search {name} must be between 0 and 9999");
    }
    Ok(year)
}

#[derive(Deserialize)]
struct TinyfishSearchResponse {
    #[serde(default)]
    results: Vec<TinyfishSearchResult>,
}

#[derive(Deserialize)]
struct TinyfishSearchResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    snippet: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    publisher: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    year: Option<u64>,
}

#[derive(Deserialize)]
struct TinyfishFetchResponse {
    #[serde(default)]
    results: Vec<TinyfishFetchResult>,
    #[serde(default)]
    errors: Vec<TinyfishFetchError>,
}

#[derive(Deserialize)]
struct TinyfishFetchResult {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    final_url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct TinyfishFetchError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    status: Option<u16>,
}

impl TinyfishFetchError {
    fn describe(self) -> String {
        let mut detail = if self.error.trim().is_empty() {
            "fetch error".to_string()
        } else {
            self.error
        };
        if let Some(status) = self.status {
            let _ = write!(detail, " (HTTP {status})");
        }
        detail
    }
}
