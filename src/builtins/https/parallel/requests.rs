use super::super::*;
use super::*;

impl HttpsProtocol {
    pub(in super::super) async fn extract_parallel(
        &self,
        url: &Url,
        api_key: &str,
    ) -> Result<Vec<u8>> {
        let response = self
            .provider_client
            .post(self.parallel_extract_url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header("x-api-key", api_key)
            .json(&json!({
                "urls": [url.as_str()],
                "advanced_settings": {
                    "full_content": {
                        "max_chars_per_result": MAX_EXTRACT_CHARS
                    }
                }
            }))
            .send()
            .await
            .context("Parallel extract request failed")?;
        let bytes = checked_provider_response(response, WebProvider::Parallel, "extract").await?;
        let response: ParallelExtractResponse =
            serde_json::from_slice(&bytes).context("Parallel returned invalid extract JSON")?;
        let Some(result) = response.results.into_iter().next() else {
            let detail = response
                .errors
                .into_iter()
                .next()
                .map(|error| error.describe())
                .unwrap_or_else(|| "no result or error detail".to_string());
            bail!("Parallel could not extract {url}: {detail}");
        };
        let content = result
            .full_content
            .and_then(nonempty)
            .or_else(|| nonempty(result.excerpts.join("\n\n")))
            .ok_or_else(|| anyhow!("Parallel returned empty extracted content for {url}"))?;
        render_provider_page(
            WebProvider::Parallel,
            url,
            result.url,
            result.title,
            result.publish_date,
            content,
        )
    }

    pub(in super::super) async fn search_parallel(
        &self,
        request: &SearchRequest,
        api_key: &str,
    ) -> Result<SearchResponse> {
        let SearchOptions::Parallel(options) = &request.options else {
            bail!("internal HTTPS provider mismatch for Parallel search");
        };
        let mut advanced = Map::new();
        advanced.insert("max_results".to_string(), json!(request.limit));
        advanced.insert(
            "excerpt_settings".to_string(),
            json!({ "max_chars_per_result": options.max_chars_per_result }),
        );
        if let Some(location) = &options.location {
            advanced.insert("location".to_string(), json!(location));
        }
        if !options.include_domains.is_empty()
            || !options.exclude_domains.is_empty()
            || options.after_date.is_some()
        {
            let mut source_policy = Map::new();
            if !options.include_domains.is_empty() {
                source_policy.insert(
                    "include_domains".to_string(),
                    json!(options.include_domains),
                );
            }
            if !options.exclude_domains.is_empty() {
                source_policy.insert(
                    "exclude_domains".to_string(),
                    json!(options.exclude_domains),
                );
            }
            if let Some(after_date) = &options.after_date {
                source_policy.insert("after_date".to_string(), json!(after_date));
            }
            advanced.insert("source_policy".to_string(), Value::Object(source_policy));
        }
        if options.max_age_seconds.is_some()
            || options.timeout_seconds.is_some()
            || options.disable_cache_fallback.is_some()
        {
            let mut fetch_policy = Map::new();
            if let Some(max_age_seconds) = options.max_age_seconds {
                fetch_policy.insert("max_age_seconds".to_string(), json!(max_age_seconds));
            }
            if let Some(timeout_seconds) = options.timeout_seconds {
                fetch_policy.insert("timeout_seconds".to_string(), json!(timeout_seconds));
            }
            if let Some(disable) = options.disable_cache_fallback {
                fetch_policy.insert("disable_cache_fallback".to_string(), json!(disable));
            }
            advanced.insert("fetch_policy".to_string(), Value::Object(fetch_policy));
        }

        let mut body = Map::new();
        body.insert("objective".to_string(), json!(request.query));
        body.insert("search_queries".to_string(), json!(options.search_queries));
        body.insert("mode".to_string(), json!(options.mode));
        body.insert("advanced_settings".to_string(), Value::Object(advanced));
        if let Some(max_chars_total) = options.max_chars_total {
            body.insert("max_chars_total".to_string(), json!(max_chars_total));
        }
        if let Some(session_id) = &options.session_id {
            body.insert("session_id".to_string(), json!(session_id));
        }
        if let Some(client_model) = &options.client_model {
            body.insert("client_model".to_string(), json!(client_model));
        }

        let response = self
            .provider_client
            .post(self.parallel_search_url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header("x-api-key", api_key)
            .json(&Value::Object(body))
            .send()
            .await
            .context("Parallel search request failed")?;
        let bytes = checked_provider_response(response, WebProvider::Parallel, "search").await?;
        let response: ParallelSearchResponse =
            serde_json::from_slice(&bytes).context("Parallel returned invalid search JSON")?;
        Ok(SearchResponse {
            provider: WebProvider::Parallel,
            request_id: response.search_id,
            warnings: response
                .warnings
                .into_iter()
                .flatten()
                .filter_map(|warning| nonempty(warning.message))
                .collect(),
            results: response
                .results
                .into_iter()
                .filter_map(|result| {
                    let url = search_result_url(result.url)?;
                    let title = result
                        .title
                        .and_then(nonempty)
                        .unwrap_or_else(|| url.clone());
                    let snippet = nonempty(result.excerpts.unwrap_or_default().join("\n\n"));
                    Some(SearchResult {
                        title,
                        url,
                        author: None,
                        published_date: result.publish_date.and_then(nonempty),
                        snippet,
                        subpages: Vec::new(),
                    })
                })
                .collect(),
        })
    }
}
