use crate::catalog::CatalogModel;
use crate::catalog::ModelCatalog;
use crate::config::{ActiveSettings, resolve_config_value};
use crate::prompts;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use rig::client::CompletionClient;
use rig::completion::{CompletionModel as RigCompletionModel, ToolDefinition};
use rig::message::{AssistantContent, Message};
use rig::providers::{anthropic, gemini, openai};
use rig::streaming::StreamedAssistantContent;
use serde_json::json;
use std::collections::HashSet;
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
}

#[async_trait]
pub trait ModelBackend: Send + Sync {
    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse>;
}

pub enum RigBackend {
    OpenAiResponses(openai::responses_api::ResponsesCompletionModel),
    OpenAiCompletions(openai::completion::CompletionModel),
    Anthropic(anthropic::completion::CompletionModel),
    Gemini(gemini::completion::CompletionModel),
}

impl RigBackend {
    pub async fn new(
        model: &CatalogModel,
        api_key: &str,
        environment: &std::collections::BTreeMap<String, String>,
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
        Ok(match model.api.as_str() {
            "openai-responses" => Self::OpenAiResponses(
                openai::Client::builder()
                    .api_key(api_key)
                    .base_url(&model.base_url)
                    .http_headers(headers)
                    .build()
                    .context("cannot initialize OpenAI provider")?
                    .completion_model(&model.id),
            ),
            "openai-completions" => Self::OpenAiCompletions(
                openai::CompletionsClient::builder()
                    .api_key(api_key)
                    .base_url(&model.base_url)
                    .http_headers(headers)
                    .build()
                    .context("cannot initialize OpenAI-compatible provider")?
                    .completion_model(&model.id),
            ),
            "anthropic-messages" => Self::Anthropic(
                anthropic::Client::builder()
                    .api_key(api_key)
                    .base_url(&model.base_url)
                    .http_headers(headers)
                    .build()
                    .context("cannot initialize Anthropic provider")?
                    .completion_model(&model.id),
            ),
            "google-generative-ai" => Self::Gemini(
                gemini::Client::builder()
                    .api_key(api_key)
                    .base_url(normalize_gemini_base_url(&model.base_url))
                    .http_headers(headers)
                    .build()
                    .context("cannot initialize Gemini provider")?
                    .completion_model(&model.id),
            ),
            api => bail!("Pi catalog API {api:?} is not supported by the Rust backend"),
        })
    }
}

pub async fn configured_backend(
    settings: &ActiveSettings,
    catalog: &ModelCatalog,
) -> Result<Option<Arc<dyn ModelBackend>>> {
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
    Ok(Some(Arc::new(
        RigBackend::new(&model, api_key, &settings.credential_environment).await?,
    )))
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
        match self {
            Self::OpenAiResponses(model) => complete_with(model, request, deltas).await,
            Self::OpenAiCompletions(model) => complete_with(model, request, deltas).await,
            Self::Anthropic(model) => complete_with(model, request, deltas).await,
            Self::Gemini(model) => complete_with(model, request, deltas).await,
        }
    }
}

async fn complete_with<M>(
    model: &M,
    request: ModelRequest,
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
        .messages(history);
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
