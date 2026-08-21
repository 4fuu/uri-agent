use crate::config::Provider;
use crate::prompts;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use rig::client::{CompletionClient, ProviderClient};
use rig::completion::{CompletionModel as RigCompletionModel, ToolDefinition};
use rig::message::{AssistantContent, Message};
use rig::providers::{anthropic, gemini, openai};
use rig::streaming::StreamedAssistantContent;
use serde_json::json;
use std::collections::HashSet;
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
    OpenAi(openai::responses_api::ResponsesCompletionModel),
    Anthropic(anthropic::completion::CompletionModel),
    Gemini(gemini::completion::CompletionModel),
}

impl RigBackend {
    pub fn from_environment(provider: Provider, model: &str) -> Result<Self> {
        Ok(match provider {
            Provider::Openai => Self::OpenAi(
                openai::Client::from_env()
                    .context("cannot initialize OpenAI provider")?
                    .completion_model(model),
            ),
            Provider::Anthropic => Self::Anthropic(
                anthropic::Client::from_env()
                    .context("cannot initialize Anthropic provider")?
                    .completion_model(model),
            ),
            Provider::Gemini => Self::Gemini(
                gemini::Client::from_env()
                    .context("cannot initialize Gemini provider")?
                    .completion_model(model),
            ),
        })
    }
}

#[async_trait]
impl ModelBackend for RigBackend {
    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse> {
        match self {
            Self::OpenAi(model) => complete_with(model, request, deltas).await,
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
    let mut stream = model
        .completion_request(prompt)
        .preamble(request.system)
        .messages(history)
        .tools(tool_definitions())
        .stream()
        .await
        .context("model request failed")?;
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
