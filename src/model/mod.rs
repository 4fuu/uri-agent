//! Model contracts and provider facade.
//!
//! Provider HTTP integration and request compatibility are implemented by the
//! Rig backend. Provider-owned security or transport boundaries live in their
//! own modules; Cloudflare AI Gateway, Codex, and experimental Antigravity
//! remain private implementation details of this facade.

mod antigravity;
mod cloudflare;
mod codebuddy;
mod codex_websocket;
mod failure;
mod request_transform;
mod retry;
mod rig_backend;

use crate::catalog::{CatalogModel, ModelCatalog, ModelLimits, ThinkingLevel};
use crate::config::{ActiveSettings, ConfigManager};
use anyhow::Result;
use async_trait::async_trait;
use rig::completion::{FinishReason, ToolDefinition};
use rig::message::{AssistantContent, Message};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(crate) use cloudflare::{
    ACCOUNT_ID_METADATA as CLOUDFLARE_ACCOUNT_ID_METADATA,
    DEFAULT_GATEWAY_ID as CLOUDFLARE_DEFAULT_GATEWAY_ID,
    GATEWAY_ID_METADATA as CLOUDFLARE_GATEWAY_ID_METADATA, PROVIDER as CLOUDFLARE_PROVIDER,
};
pub(crate) use failure::{
    ModelFailure, ModelFailureKind, ModelFailurePhase, looks_like_context_overflow,
};
#[cfg(test)]
pub(crate) use retry::MAX_RETRY_AFTER;
pub(crate) use retry::{model_retry_delay, model_retry_policy, model_retry_reason};

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

pub async fn configured_backend(
    settings: &ActiveSettings,
    catalog: &ModelCatalog,
    session_id: Option<&str>,
    manager: Arc<ConfigManager>,
) -> Result<Option<(Arc<dyn ModelBackend>, ModelLimits)>> {
    if !settings.model_configured() || settings.api_key.is_none() {
        return Ok(None);
    }
    let model = settings.catalog_model(catalog).await.ok_or_else(|| {
        anyhow::anyhow!(
            "model {}/{} is not available in the runnable Pi catalog",
            settings.provider,
            settings.model
        )
    })?;
    let limits = model.limits();
    let backend: Arc<dyn ModelBackend> = if model.provider == cloudflare::PROVIDER {
        Arc::new(cloudflare::CloudflareBackend::new(
            model,
            settings.clone(),
            session_id,
            manager,
        ))
    } else if model.provider == codebuddy::PROVIDER {
        Arc::new(codebuddy::CodeBuddyBackend::new(
            model,
            settings.clone(),
            session_id,
            manager,
        ))
    } else {
        rig_backend::deferred_backend(model, settings.clone(), session_id, manager)
    };
    Ok(Some((backend, limits)))
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
