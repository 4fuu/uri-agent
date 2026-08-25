//! Model contracts and provider facade.
//!
//! Provider HTTP integration and request compatibility are implemented by the
//! Rig backend, while the Codex and experimental Antigravity streaming
//! transports remain private to it.

mod antigravity;
mod codex_websocket;
mod failure;
mod request_transform;
mod rig_backend;

use crate::catalog::{CatalogModel, ThinkingLevel};
use crate::prompts;
use anyhow::Result;
use async_trait::async_trait;
use rig::completion::{FinishReason, ToolDefinition};
use rig::message::{AssistantContent, Message};
use serde_json::json;
use tokio::sync::mpsc;

pub(crate) use failure::{
    ModelFailure, ModelFailureKind, ModelFailurePhase, looks_like_context_overflow,
};
pub use rig_backend::configured_backend;

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
    pub estimated_context: usize,
    pub max_output_tokens: Option<usize>,
}

pub struct ModelResponse {
    pub content: Vec<AssistantContent>,
    pub usage: Option<rig::completion::Usage>,
    pub context_tokens: Option<usize>,
    pub finish_reason: Option<FinishReason>,
}

#[async_trait]
pub trait ModelBackend: Send + Sync {
    async fn prepare(&self) -> Result<()> {
        Ok(())
    }

    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse>;

    fn accepts_image_input(&self) -> bool {
        false
    }

    fn desired_max_output_tokens(&self) -> usize {
        0
    }
}

pub(crate) fn clamp_thinking_level(
    model: &CatalogModel,
    requested: ThinkingLevel,
) -> ThinkingLevel {
    if model.supports_thinking_level(requested) {
        return requested;
    }
    let index = ThinkingLevel::ALL
        .iter()
        .position(|level| *level == requested)
        .unwrap_or_default();
    ThinkingLevel::ALL[index..]
        .iter()
        .copied()
        .find(|level| model.supports_thinking_level(*level))
        .or_else(|| {
            ThinkingLevel::ALL[..index]
                .iter()
                .rev()
                .copied()
                .find(|level| model.supports_thinking_level(*level))
        })
        .unwrap_or(ThinkingLevel::Off)
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    let parameters = json!({
        "type": "object",
        "properties": {
            "uri": {
                "type": "string",
                "description": "Protocol address in the custom form <protocol>://<opaque-target>. It is not an RFC URL and is passed to the selected protocol unchanged."
            },
            "body": {
                "type": "object",
                "description": "Required envelope for the protocol-specific body. Use kind `none` with an empty value when the protocol takes no body, `text` for a literal string body, or `json` with the complete JSON serialization of any JSON body.",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["none", "text", "json"],
                        "description": "How URI Agent decodes value before protocol dispatch."
                    },
                    "value": {
                        "type": "string",
                        "description": "Empty for `none`, literal protocol text for `text`, or complete serialized JSON for `json`."
                    }
                },
                "required": ["kind", "value"],
                "additionalProperties": false
            }
        },
        "required": ["uri", "body"],
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
mod tests;
