//! Model contracts and provider facade.
//!
//! Provider HTTP integration and request compatibility are implemented by the
//! Rig backend, while the Codex and experimental Antigravity streaming
//! transports remain private to it.

mod antigravity;
mod codex_websocket;
mod failure;
mod request_transform;
mod retry;
mod rig_backend;

use crate::catalog::{CatalogModel, ThinkingLevel};
use anyhow::Result;
use async_trait::async_trait;
use rig::completion::{FinishReason, ToolDefinition};
use rig::message::{AssistantContent, Message};
use tokio::sync::mpsc;

pub(crate) use failure::{
    ModelFailure, ModelFailureKind, ModelFailurePhase, looks_like_context_overflow,
};
#[cfg(test)]
pub(crate) use retry::MAX_RETRY_AFTER;
pub(crate) use retry::{model_retry_delay, model_retry_policy, model_retry_reason};
pub use rig_backend::configured_backend;

#[derive(Clone, Debug)]
pub enum ModelDelta {
    Text(String),
    Reasoning(String),
    ToolCall(String),
}

#[derive(Clone)]
pub struct ModelRequest {
    pub system: String,
    pub history: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
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

#[cfg(test)]
mod tests;
