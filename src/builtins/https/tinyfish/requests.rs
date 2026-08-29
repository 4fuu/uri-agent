use super::super::*;
use super::*;

impl HttpsProtocol {
    pub(in super::super) async fn extract_tinyfish(
        &self,
        url: &Url,
        api_key: &str,
    ) -> Result<Vec<u8>> {
        let response = self
            .provider_client
            .post(self.tinyfish_fetch_url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header("x-api-key", api_key)
            .json(&json!({
                "urls": [url.as_str()],
                "format": "markdown"
            }))
            .send()
            .await
            .context("TinyFish fetch request failed")?;
        let bytes = checked_provider_response(response, WebProvider::Tinyfish, "fetch").await?;
        let response: TinyfishFetchResponse =
            serde_json::from_slice(&bytes).context("TinyFish returned invalid fetch JSON")?;
        let Some(result) = response.results.into_iter().next() else {
            let detail = response
                .errors
                .into_iter()
                .next()
                .map(TinyfishFetchError::describe)
                .unwrap_or_else(|| "no result or error detail".to_string());
            bail!("TinyFish could not extract {url}: {detail}");
        };
        let content = result
            .text
            .and_then(nonempty)
            .ok_or_else(|| anyhow!("TinyFish returned empty extracted content for {url}"))?;
        render_provider_page(
            WebProvider::Tinyfish,
            url,
            result.final_url.or(result.url),
            result.title,
            result.published_date,
            content,
        )
    }

    pub(in super::super) async fn search_tinyfish(
        &self,
        request: &SearchRequest,
        api_key: &str,
    ) -> Result<SearchResponse> {
        let SearchOptions::Tinyfish(options) = &request.options else {
            bail!("internal HTTPS provider mismatch for TinyFish search");
        };
        // TinyFish has no result-count parameter; one request returns a single
        // page, so follow pages until the requested limit is covered.
        let mut results = Vec::new();
        let mut page = 0_u32;
        loop {
            let response = self
                .request_tinyfish_search(request, options, page, api_key)
                .await?;
            let received = response.results.len();
            results.extend(response.results.into_iter().filter_map(|result| {
                let url = search_result_url(result.url)?;
                let title = result
                    .title
                    .and_then(nonempty)
                    .unwrap_or_else(|| url.clone());
                let author = nonempty(result.authors.join(", "))
                    .or_else(|| result.publisher.and_then(nonempty));
                let published_date = result
                    .date
                    .and_then(nonempty)
                    .or_else(|| result.year.map(|year| year.to_string()));
                Some(SearchResult {
                    title,
                    url,
                    author,
                    published_date,
                    snippet: result.snippet.and_then(nonempty),
                    subpages: Vec::new(),
                })
            }));
            if results.len() >= request.limit
                || received < TINYFISH_PAGE_SIZE
                || page >= TINYFISH_MAX_PAGE
            {
                break;
            }
            page += 1;
        }
        Ok(SearchResponse {
            provider: WebProvider::Tinyfish,
            request_id: None,
            warnings: Vec::new(),
            results,
        })
    }

    async fn request_tinyfish_search(
        &self,
        request: &SearchRequest,
        options: &TinyfishSearchOptions,
        page: u32,
        api_key: &str,
    ) -> Result<TinyfishSearchResponse> {
        let mut url = self.tinyfish_search_url.clone();
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("query", &request.query);
            if let Some(purpose) = &options.purpose {
                pairs.append_pair("purpose", purpose);
            }
            if let Some(location) = &options.location {
                pairs.append_pair("location", location);
            }
            if let Some(language) = &options.language {
                pairs.append_pair("language", language);
            }
            if !options.include_domains.is_empty() {
                pairs.append_pair("include_domains", &options.include_domains.join(","));
            }
            if !options.exclude_domains.is_empty() {
                pairs.append_pair("exclude_domains", &options.exclude_domains.join(","));
            }
            if let Some(recency_minutes) = options.recency_minutes {
                pairs.append_pair("recency_minutes", &recency_minutes.to_string());
            }
            if let Some(after_date) = &options.after_date {
                pairs.append_pair("after_date", after_date);
            }
            if let Some(before_date) = &options.before_date {
                pairs.append_pair("before_date", before_date);
            }
            if options.domain_type != "web" {
                pairs.append_pair("domain_type", &options.domain_type);
            }
            if let Some(pub_year_min) = options.pub_year_min {
                pairs.append_pair("pub_year_min", &pub_year_min.to_string());
            }
            if let Some(pub_year_max) = options.pub_year_max {
                pairs.append_pair("pub_year_max", &pub_year_max.to_string());
            }
            if page > 0 {
                pairs.append_pair("page", &page.to_string());
            }
        }
        let response = self
            .provider_client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header("x-api-key", api_key)
            .send()
            .await
            .context("TinyFish search request failed")?;
        let bytes = checked_provider_response(response, WebProvider::Tinyfish, "search").await?;
        serde_json::from_slice(&bytes).context("TinyFish returned invalid search JSON")
    }
}
