use super::super::*;
use super::*;

impl HttpsProtocol {
    pub(in super::super) async fn extract_exa(&self, url: &Url, api_key: &str) -> Result<Vec<u8>> {
        let response = self
            .provider_client
            .post(self.exa_extract_url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header("x-api-key", api_key)
            .json(&json!({
                "urls": [url.as_str()],
                "text": true
            }))
            .send()
            .await
            .context("Exa contents request failed")?;
        let bytes = checked_provider_response(response, WebProvider::Exa, "contents").await?;
        let response: ExaContentsResponse =
            serde_json::from_slice(&bytes).context("Exa returned invalid contents JSON")?;
        let Some(result) = response.results.into_iter().next() else {
            let detail = response
                .statuses
                .into_iter()
                .find(|status| status.status != "success")
                .map(ExaContentsStatus::describe)
                .unwrap_or_else(|| "no result or status detail".to_string());
            bail!("Exa could not extract {url}: {detail}");
        };
        let content = result
            .text
            .and_then(nonempty)
            .ok_or_else(|| anyhow!("Exa returned empty extracted content for {url}"))?;
        render_provider_page(
            WebProvider::Exa,
            url,
            result.url,
            result.title,
            result.published_date,
            content,
        )
    }

    pub(in super::super) async fn search_exa(
        &self,
        request: &SearchRequest,
        api_key: &str,
    ) -> Result<SearchResponse> {
        let SearchOptions::Exa(options) = &request.options else {
            bail!("internal HTTPS provider mismatch for Exa search");
        };
        let content = match options.content {
            ExaContent::Highlights => options
                .max_characters
                .map(|limit| json!({ "maxCharacters": limit }))
                .unwrap_or(Value::Bool(true)),
            ExaContent::Text => options
                .max_characters
                .map(|limit| json!({ "maxCharacters": limit }))
                .unwrap_or(Value::Bool(true)),
            ExaContent::Summary => Value::Object(Map::new()),
        };
        let mut contents = Map::new();
        contents.insert(options.content.api_field().to_string(), content);
        if let Some(max_age_hours) = options.max_age_hours {
            contents.insert("maxAgeHours".to_string(), json!(max_age_hours));
        }
        if let Some(timeout) = options.livecrawl_timeout {
            contents.insert("livecrawlTimeout".to_string(), json!(timeout));
        }
        if let Some(subpages) = options.subpages {
            contents.insert("subpages".to_string(), json!(subpages));
        }
        if !options.subpage_targets.is_empty() {
            contents.insert("subpageTarget".to_string(), json!(options.subpage_targets));
        }

        let mut body = Map::new();
        body.insert("query".to_string(), json!(request.query));
        body.insert("numResults".to_string(), json!(request.limit));
        body.insert("type".to_string(), json!(options.search_type));
        body.insert("contents".to_string(), Value::Object(contents));
        if let Some(category) = &options.category {
            body.insert("category".to_string(), json!(category));
        }
        if let Some(location) = &options.location {
            body.insert("userLocation".to_string(), json!(location));
        }
        if !options.include_domains.is_empty() {
            body.insert("includeDomains".to_string(), json!(options.include_domains));
        }
        if !options.exclude_domains.is_empty() {
            body.insert("excludeDomains".to_string(), json!(options.exclude_domains));
        }
        if let Some(date) = &options.start_published_date {
            body.insert("startPublishedDate".to_string(), json!(date));
        }
        if let Some(date) = &options.end_published_date {
            body.insert("endPublishedDate".to_string(), json!(date));
        }
        if let Some(moderation) = options.moderation {
            body.insert("moderation".to_string(), json!(moderation));
        }
        if !options.additional_queries.is_empty() {
            body.insert(
                "additionalQueries".to_string(),
                json!(options.additional_queries),
            );
        }
        if let Some(system_prompt) = &options.system_prompt {
            body.insert("systemPrompt".to_string(), json!(system_prompt));
        }

        let response = self
            .provider_client
            .post(self.exa_search_url.clone())
            .header("x-api-key", api_key)
            .json(&Value::Object(body))
            .send()
            .await
            .context("Exa search request failed")?;
        let bytes = checked_provider_response(response, WebProvider::Exa, "search").await?;
        let response: ExaSearchResponse =
            serde_json::from_slice(&bytes).context("Exa returned invalid search JSON")?;
        Ok(SearchResponse {
            provider: WebProvider::Exa,
            request_id: response.request_id,
            warnings: Vec::new(),
            results: response
                .results
                .into_iter()
                .filter_map(|result| {
                    let url = search_result_url(result.url)?;
                    let title = result
                        .title
                        .and_then(nonempty)
                        .unwrap_or_else(|| url.clone());
                    let snippet = result
                        .summary
                        .and_then(nonempty)
                        .or_else(|| result.text.and_then(nonempty))
                        .or_else(|| nonempty(result.highlights.unwrap_or_default().join(" ")));
                    let subpages = result
                        .subpages
                        .into_iter()
                        .filter_map(|subpage| {
                            let url = search_result_url(subpage.url)?;
                            let title = subpage
                                .title
                                .and_then(nonempty)
                                .unwrap_or_else(|| url.clone());
                            Some(SearchSubpage { title, url })
                        })
                        .collect();
                    Some(SearchResult {
                        title,
                        url,
                        author: result.author.and_then(nonempty),
                        published_date: result.published_date.and_then(nonempty),
                        snippet,
                        subpages,
                    })
                })
                .collect(),
        })
    }
}
