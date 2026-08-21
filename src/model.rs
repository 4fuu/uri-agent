use crate::catalog::{CatalogModel, ModelCatalog, ModelLimits};
use crate::compaction;
use crate::config::{ActiveSettings, AuthKind, resolve_config_value};
use crate::prompts;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use rig::client::CompletionClient;
use rig::completion::{CompletionModel as RigCompletionModel, ToolDefinition};
use rig::http_client::HttpClientExt;
use rig::message::{AssistantContent, Message};
use rig::providers::{anthropic, gemini, openai};
use rig::streaming::StreamedAssistantContent;
use serde_json::json;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum ModelDelta {
    Text(String),
    Reasoning(String),
}

#[derive(Clone)]
pub struct ModelRequest {
    pub system: String,
    pub history: Vec<Message>,
    pub tools: bool,
}

pub struct ModelResponse {
    pub content: Vec<AssistantContent>,
    pub usage: Option<rig::completion::Usage>,
}

/// Mirrors pi's `clampMaxTokensToContext`: the catalog's `maxTokens` capped by
/// the room left in the context window after the estimated prompt and a fixed
/// safety margin.
const CONTEXT_SAFETY_TOKENS: usize = 4_096;

fn clamp_max_tokens_to_context(limits: &ModelLimits, estimated_context: usize) -> u64 {
    if limits.context_window == 0 {
        return limits.max_tokens.max(1);
    }
    let available = limits
        .context_window
        .saturating_sub(estimated_context)
        .saturating_sub(CONTEXT_SAFETY_TOKENS)
        .max(1);
    limits.max_tokens.min(available as u64).max(1)
}

#[async_trait]
pub trait ModelBackend: Send + Sync {
    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse>;
}

#[derive(Clone)]
pub(crate) struct AuthClient {
    inner: reqwest::Client,
    strip_x_api_key: bool,
}

impl HttpClientExt for AuthClient {
    fn send<T, U>(
        &self,
        mut req: http::Request<T>,
    ) -> impl Future<
        Output = rig::http_client::Result<http::Response<rig::http_client::LazyBody<U>>>,
    > + Send
    + 'static
    where
        T: Into<bytes::Bytes> + Send,
        U: From<bytes::Bytes> + Send + 'static,
    {
        if self.strip_x_api_key {
            req.headers_mut().remove("x-api-key");
        }
        HttpClientExt::send(&self.inner, req)
    }

    fn send_multipart<U>(
        &self,
        mut req: http::Request<rig::http_client::multipart::MultipartForm>,
    ) -> impl Future<
        Output = rig::http_client::Result<http::Response<rig::http_client::LazyBody<U>>>,
    > + Send
    + 'static
    where
        U: From<bytes::Bytes> + Send + 'static,
    {
        if self.strip_x_api_key {
            req.headers_mut().remove("x-api-key");
        }
        HttpClientExt::send_multipart(&self.inner, req)
    }

    fn send_streaming<T>(
        &self,
        mut req: http::Request<T>,
    ) -> impl Future<Output = rig::http_client::Result<rig::http_client::StreamingResponse>> + Send
    where
        T: Into<bytes::Bytes> + Send,
    {
        if self.strip_x_api_key {
            req.headers_mut().remove("x-api-key");
        }
        HttpClientExt::send_streaming(&self.inner, req)
    }
}

pub(crate) struct RigBackend {
    client: RigClient,
    limits: ModelLimits,
}

pub(crate) enum RigClient {
    OpenAiResponses(openai::responses_api::ResponsesCompletionModel),
    OpenAiCompletions(openai::completion::CompletionModel),
    Anthropic(anthropic::completion::CompletionModel<AuthClient>),
    Gemini(gemini::completion::CompletionModel),
}

impl RigBackend {
    pub async fn new(
        model: &CatalogModel,
        api_key: &str,
        environment: &std::collections::BTreeMap<String, String>,
        auth_kind: AuthKind,
    ) -> Result<Self> {
        let mut headers = resolved_headers(model, environment).await?;
        if model
            .metadata
            .get("authHeader")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            headers.entry(http::header::AUTHORIZATION).or_insert(
                HeaderValue::from_str(&format!("Bearer {api_key}"))
                    .context("invalid API key for Authorization header")?,
            );
        }
        let anthropic_oauth = auth_kind == AuthKind::Oauth && model.api == "anthropic-messages";
        if anthropic_oauth {
            headers.insert(
                http::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}"))
                    .context("invalid OAuth token for Authorization header")?,
            );
            headers.insert(
                HeaderName::from_static("x-app"),
                HeaderValue::from_static("cli"),
            );
            headers.insert(
                http::header::USER_AGENT,
                HeaderValue::from_static("claude-cli/2.1.32"),
            );
        }
        let limits = model.limits();
        let client = match model.api.as_str() {
            "openai-responses" => RigClient::OpenAiResponses(
                openai::Client::builder()
                    .api_key(api_key)
                    .base_url(&model.base_url)
                    .http_headers(headers)
                    .build()
                    .context("cannot initialize OpenAI provider")?
                    .completion_model(&model.id),
            ),
            "openai-completions" => RigClient::OpenAiCompletions(
                openai::CompletionsClient::builder()
                    .api_key(api_key)
                    .base_url(&model.base_url)
                    .http_headers(headers)
                    .build()
                    .context("cannot initialize OpenAI-compatible provider")?
                    .completion_model(&model.id),
            ),
            "anthropic-messages" => {
                let mut builder = anthropic::Client::builder()
                    .api_key(api_key)
                    .base_url(&model.base_url)
                    .http_headers(headers)
                    .http_client(AuthClient {
                        inner: reqwest::Client::new(),
                        strip_x_api_key: anthropic_oauth,
                    });
                if anthropic_oauth {
                    builder = builder
                        .anthropic_beta("claude-code-20250219")
                        .anthropic_beta("oauth-2025-04-20");
                }
                RigClient::Anthropic(
                    builder
                        .build()
                        .context("cannot initialize Anthropic provider")?
                        .completion_model(&model.id),
                )
            }
            "google-generative-ai" => RigClient::Gemini(
                gemini::Client::builder()
                    .api_key(api_key)
                    .base_url(normalize_gemini_base_url(&model.base_url))
                    .http_headers(headers)
                    .build()
                    .context("cannot initialize Gemini provider")?
                    .completion_model(&model.id),
            ),
            api => bail!("Pi catalog API {api:?} is not supported by the Rust backend"),
        };
        Ok(Self { client, limits })
    }
}

pub async fn configured_backend(
    settings: &ActiveSettings,
    catalog: &ModelCatalog,
) -> Result<Option<(Arc<dyn ModelBackend>, ModelLimits)>> {
    if !settings.model_configured() {
        return Ok(None);
    }
    let Some(api_key) = settings.api_key.as_deref() else {
        return Ok(None);
    };
    let model = settings.catalog_model(catalog).await.ok_or_else(|| {
        anyhow::anyhow!(
            "model {}/{} is not available in the runnable Pi catalog",
            settings.provider,
            settings.model
        )
    })?;
    let backend = RigBackend::new(
        &model,
        api_key,
        &settings.credential_environment,
        settings.auth_kind,
    )
    .await?;
    let limits = backend.limits;
    Ok(Some((Arc::new(backend), limits)))
}

async fn resolved_headers(
    model: &CatalogModel,
    environment: &std::collections::BTreeMap<String, String>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in &model.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).with_context(|| {
            format!(
                "invalid header name {name:?} for {}/{}",
                model.provider, model.id
            )
        })?;
        let value = resolve_config_value(value, environment).await?;
        headers.insert(
            name,
            HeaderValue::from_str(&value).context("invalid configured model header value")?,
        );
    }
    Ok(headers)
}

fn normalize_gemini_base_url(base_url: &str) -> &str {
    base_url
        .trim_end_matches('/')
        .strip_suffix("/v1beta")
        .unwrap_or_else(|| base_url.trim_end_matches('/'))
}

#[async_trait]
impl ModelBackend for RigBackend {
    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse> {
        let estimated = compaction::estimate_tokens(&request.system, &request.history);
        let max_tokens = clamp_max_tokens_to_context(&self.limits, estimated);
        match &self.client {
            RigClient::OpenAiResponses(model) => {
                complete_with(model, request, max_tokens, deltas).await
            }
            RigClient::OpenAiCompletions(model) => {
                complete_with(model, request, max_tokens, deltas).await
            }
            RigClient::Anthropic(model) => complete_with(model, request, max_tokens, deltas).await,
            RigClient::Gemini(model) => complete_with(model, request, max_tokens, deltas).await,
        }
    }
}

async fn complete_with<M>(
    model: &M,
    request: ModelRequest,
    max_tokens: u64,
    deltas: mpsc::UnboundedSender<ModelDelta>,
) -> Result<ModelResponse>
where
    M: RigCompletionModel + Clone,
{
    let mut history = request.history;
    let prompt = history
        .pop()
        .ok_or_else(|| anyhow::anyhow!("model request has no user or tool-result message"))?;
    let mut completion = model
        .completion_request(prompt)
        .preamble(request.system)
        .messages(history)
        .max_tokens(max_tokens);
    if request.tools {
        completion = completion.tools(tool_definitions());
    }
    let mut stream = completion.stream().await.context("model request failed")?;
    let mut reasoning_deltas = HashSet::new();
    while let Some(event) = stream.next().await {
        match event.context("model stream failed")? {
            StreamedAssistantContent::Text(text) => {
                let _ = deltas.send(ModelDelta::Text(text.text));
            }
            StreamedAssistantContent::ReasoningDelta { id, reasoning, .. } => {
                reasoning_deltas.insert(id);
                let _ = deltas.send(ModelDelta::Reasoning(reasoning));
            }
            StreamedAssistantContent::Reasoning { id, reasoning }
                if !reasoning_deltas.contains(&id) =>
            {
                let text = reasoning.display_text();
                if !text.is_empty() {
                    let _ = deltas.send(ModelDelta::Reasoning(text));
                }
            }
            StreamedAssistantContent::ToolCall { .. }
            | StreamedAssistantContent::ToolCallDelta { .. }
            | StreamedAssistantContent::Reasoning { .. }
            | StreamedAssistantContent::Final(_)
            | StreamedAssistantContent::Unknown(_) => {}
        }
    }
    if stream.choice.is_empty() {
        bail!("model returned no assistant content");
    }
    Ok(ModelResponse {
        content: stream.choice.clone(),
        usage: stream.response.as_ref().map(|final_| final_.usage),
    })
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    let parameters = json!({
        "type": "object",
        "properties": {
            "uri": {
                "type": "string",
                "description": "Custom protocol address in the form <protocol>://<opaque-target>."
            },
            "body": {
                "description": "Optional protocol-specific payload. Any JSON value is accepted."
            }
        },
        "required": ["uri"],
        "additionalProperties": false
    });
    vec![
        ToolDefinition {
            name: "read".to_string(),
            description: prompts::READ_TOOL_DESCRIPTION.to_string(),
            parameters: parameters.clone(),
        },
        ToolDefinition {
            name: "exec".to_string(),
            description: prompts::EXEC_TOOL_DESCRIPTION.to_string(),
            parameters,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_tokens_follows_pi_catalog_and_context_clamping() {
        let limits = ModelLimits {
            context_window: 100_000,
            max_tokens: 32_000,
            cost: Default::default(),
        };
        assert_eq!(clamp_max_tokens_to_context(&limits, 10_000), 32_000);
        // 100_000 - 80_000 - 4_096 safety margin = 15_904 available.
        assert_eq!(clamp_max_tokens_to_context(&limits, 80_000), 15_904);
        // Never drops below one even when the estimate exceeds the window.
        assert_eq!(clamp_max_tokens_to_context(&limits, 200_000), 1);
        let unbounded = ModelLimits {
            context_window: 0,
            ..limits
        };
        assert_eq!(clamp_max_tokens_to_context(&unbounded, 200_000), 32_000);
    }

    #[test]
    fn model_only_sees_two_tools_and_body_is_unconstrained() {
        let tools = tool_definitions();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "exec"]
        );
        assert!(
            tools[0].parameters["properties"]["body"]
                .get("type")
                .is_none()
        );
    }
}
