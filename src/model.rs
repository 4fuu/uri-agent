use crate::catalog::{CatalogModel, ModelCatalog, ModelLimits, ThinkingLevel};
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
use serde_json::{Map, Value, json};
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

    fn accepts_image_input(&self) -> bool {
        false
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

#[derive(Clone, Debug)]
struct ModelRequestTransform {
    model: CatalogModel,
    thinking: ThinkingLevel,
    session_id: Option<String>,
}

impl ModelRequestTransform {
    fn transform_bytes(&self, bytes: bytes::Bytes) -> bytes::Bytes {
        let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
            return bytes;
        };
        let Some(body) = value.as_object_mut() else {
            return bytes;
        };
        match self.model.api.as_str() {
            "openai-responses" => self.openai_responses(body),
            "openai-completions" => self.openai_completions(body),
            "anthropic-messages" => self.anthropic(body),
            "google-generative-ai" => self.google(body),
            _ => {}
        }
        serde_json::to_vec(&value).map_or(bytes, bytes::Bytes::from)
    }

    fn transform_headers(&self, headers: &mut HeaderMap) {
        self.apply_session_affinity(headers);
        if self.model.api != "anthropic-messages" {
            return;
        }
        let mut beta = |value: &'static str| {
            headers.append(
                HeaderName::from_static("anthropic-beta"),
                HeaderValue::from_static(value),
            );
        };
        if !self.compat_bool("supportsEagerToolInputStreaming", true) {
            beta("fine-grained-tool-streaming-2025-05-14");
        }
        if self.model.reasoning()
            && self.thinking.enabled()
            && !self.compat_bool("forceAdaptiveThinking", false)
        {
            beta("interleaved-thinking-2025-05-14");
        }
        if self
            .model
            .compat("allowedFallbackModels")
            .and_then(Value::as_array)
            .is_some_and(|models| !models.is_empty())
        {
            beta("server-side-fallback-2026-07-01");
        }
    }

    fn apply_session_affinity(&self, headers: &mut HeaderMap) {
        let Some(session_id) = self.session_id.as_deref() else {
            return;
        };
        if self.model.api == "anthropic-messages" {
            if self.compat_bool("sendSessionAffinityHeaders", false)
                && let Ok(value) = HeaderValue::from_str(session_id)
            {
                headers.insert(HeaderName::from_static("x-session-affinity"), value);
            }
            return;
        }
        let enabled = self.model.api == "openai-responses"
            || (self.model.api == "openai-completions"
                && self.compat_bool("sendSessionAffinityHeaders", false));
        if !enabled {
            return;
        }
        let default = if self.model.provider == "openrouter"
            || self.model.base_url.contains("openrouter.ai")
        {
            "openrouter"
        } else {
            "openai"
        };
        let format = self
            .model
            .compat("sessionAffinityFormat")
            .and_then(Value::as_str)
            .unwrap_or(default);
        let insert = |headers: &mut HeaderMap, name: &'static str| {
            if let Ok(value) = HeaderValue::from_str(session_id) {
                headers.insert(HeaderName::from_static(name), value);
            }
        };
        if format == "openrouter" {
            insert(headers, "x-session-id");
        } else {
            if format == "openai" {
                insert(headers, "session_id");
            }
            insert(headers, "x-client-request-id");
            if self.model.api == "openai-completions" {
                insert(headers, "x-session-affinity");
            }
        }
    }

    fn compat_bool(&self, key: &str, default: bool) -> bool {
        self.model
            .compat(key)
            .and_then(Value::as_bool)
            .unwrap_or(default)
    }

    fn mapped_level(&self) -> Value {
        self.model
            .thinking_level(self.thinking)
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| Value::String(self.thinking.as_str().to_string()))
    }

    fn off_mapping(&self) -> Option<Value> {
        match self.model.thinking_level(ThinkingLevel::Off) {
            Some(Value::Null) => None,
            Some(value) => Some(value.clone()),
            None => Some(Value::String("none".to_string())),
        }
    }

    fn apply_sampling_params(&self, body: &mut Map<String, Value>) {
        if let Some(params) = self.model.sampling_params() {
            body.extend(params.clone());
        }
    }

    fn apply_tool_strictness(&self, body: &mut Map<String, Value>, default: bool) {
        let supported = self.compat_bool("supportsStrictMode", default);
        let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
            return;
        };
        for tool in tools {
            let Some(tool) = tool.as_object_mut() else {
                continue;
            };
            let function_tool = tool.get("type").and_then(Value::as_str) == Some("function");
            let function = if tool.contains_key("function") {
                tool.get_mut("function").and_then(Value::as_object_mut)
            } else if function_tool {
                Some(tool)
            } else {
                None
            };
            let Some(function) = function else { continue };
            if supported {
                function.entry("strict").or_insert(Value::Bool(false));
            } else {
                function.remove("strict");
            }
        }
    }

    fn openai_responses(&self, body: &mut Map<String, Value>) {
        body.insert("store".to_string(), Value::Bool(false));
        if let Some(session_id) = &self.session_id {
            body.insert(
                "prompt_cache_key".to_string(),
                Value::String(session_id.clone()),
            );
        }
        self.apply_tool_strictness(body, false);
        if self.model.reasoning() {
            if self.thinking.enabled() {
                body.insert(
                    "reasoning".to_string(),
                    json!({"effort": self.mapped_level(), "summary": "auto"}),
                );
                body.insert(
                    "include".to_string(),
                    json!(["reasoning.encrypted_content"]),
                );
            } else if self.model.provider != "github-copilot"
                && let Some(off) = self.off_mapping()
            {
                body.insert("reasoning".to_string(), json!({"effort": off}));
            }
            if self.model.provider == "xai" {
                body.insert(
                    "include".to_string(),
                    json!(["reasoning.encrypted_content"]),
                );
            }
        }
        self.apply_sampling_params(body);
    }

    fn completions_compat(&self, key: &str, default: bool) -> bool {
        self.model
            .compat(key)
            .and_then(Value::as_bool)
            .unwrap_or(default)
    }

    fn nonstandard_completions_provider(&self) -> bool {
        let provider = self.model.provider.as_str();
        let url = self.model.base_url.to_ascii_lowercase();
        matches!(
            provider,
            "nvidia"
                | "cerebras"
                | "xai"
                | "together"
                | "deepseek"
                | "zai"
                | "zai-coding-cn"
                | "moonshotai"
                | "moonshotai-cn"
                | "opencode"
                | "cloudflare-workers-ai"
                | "cloudflare-ai-gateway"
                | "ant-ling"
        ) || [
            "integrate.api.nvidia.com",
            "cerebras.ai",
            "api.x.ai",
            "api.together.",
            "chutes.ai",
            "deepseek.com",
            "api.z.ai",
            "open.bigmodel.cn",
            "api.moonshot.",
            "opencode.ai",
            "api.cloudflare.com",
            "gateway.ai.cloudflare.com",
            "api.ant-ling.com",
        ]
        .iter()
        .any(|needle| url.contains(needle))
    }

    fn uses_max_tokens(&self) -> bool {
        if let Some(field) = self.model.compat("maxTokensField").and_then(Value::as_str) {
            return field == "max_tokens";
        }
        let provider = self.model.provider.as_str();
        let url = self.model.base_url.to_ascii_lowercase();
        matches!(
            provider,
            "deepseek"
                | "moonshotai"
                | "moonshotai-cn"
                | "cloudflare-ai-gateway"
                | "together"
                | "nvidia"
                | "ant-ling"
                | "zai"
                | "zai-coding-cn"
        ) || [
            "chutes.ai",
            "deepseek.com",
            "api.moonshot.",
            "gateway.ai.cloudflare.com",
            "api.together.",
            "integrate.api.nvidia.com",
            "api.ant-ling.com",
            "api.z.ai",
            "open.bigmodel.cn",
        ]
        .iter()
        .any(|needle| url.contains(needle))
    }

    fn thinking_format(&self) -> &str {
        if let Some(format) = self.model.compat("thinkingFormat").and_then(Value::as_str) {
            return format;
        }
        match self.model.provider.as_str() {
            "deepseek" => "deepseek",
            "zai" | "zai-coding-cn" => "zai",
            "together" => "together",
            "ant-ling" => "ant-ling",
            "openrouter" => "openrouter",
            _ => "openai",
        }
    }

    fn openai_completions(&self, body: &mut Map<String, Value>) {
        if self.model.base_url.contains("api.openai.com")
            && let Some(session_id) = &self.session_id
        {
            body.insert(
                "prompt_cache_key".to_string(),
                Value::String(session_id.clone()),
            );
        }
        if self.uses_max_tokens() {
            if let Some(max_tokens) = body.remove("max_completion_tokens") {
                body.insert("max_tokens".to_string(), max_tokens);
            }
        } else if let Some(max_tokens) = body.remove("max_tokens") {
            body.insert("max_completion_tokens".to_string(), max_tokens);
        }
        if !self.completions_compat("supportsUsageInStreaming", true) {
            body.remove("stream_options");
        }
        if self.completions_compat("supportsStore", !self.nonstandard_completions_provider()) {
            body.insert("store".to_string(), Value::Bool(false));
        } else {
            body.remove("store");
        }
        if !self.compat_bool("supportsTemperature", true) {
            body.remove("temperature");
        }
        let strict_default = !matches!(
            self.model.provider.as_str(),
            "moonshotai" | "moonshotai-cn" | "together" | "cloudflare-ai-gateway" | "nvidia"
        );
        self.apply_tool_strictness(body, strict_default);
        self.apply_developer_role(body);
        self.apply_openai_cache_control(body);
        self.apply_reasoning_replay_compat(body);
        self.apply_completions_thinking(body);
        if self.compat_bool("zaiToolStream", false) && body.contains_key("tools") {
            body.insert("tool_stream".to_string(), Value::Bool(true));
        }
        self.apply_sampling_params(body);
    }

    fn apply_reasoning_replay_compat(&self, body: &mut Map<String, Value>) {
        let required = self.completions_compat(
            "requiresReasoningContentOnAssistantMessages",
            self.model.provider == "deepseek"
                || self
                    .model
                    .base_url
                    .to_ascii_lowercase()
                    .contains("deepseek.com"),
        );
        if !required || !self.model.reasoning() {
            return;
        }
        let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
            return;
        };
        for message in messages.iter_mut().filter_map(Value::as_object_mut) {
            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                message
                    .entry("reasoning_content")
                    .or_insert_with(|| Value::String(String::new()));
            }
        }
    }

    fn apply_developer_role(&self, body: &mut Map<String, Value>) {
        if !self.model.reasoning() {
            return;
        }
        let openrouter_model = self.model.provider == "openrouter"
            && (self.model.id.starts_with("anthropic/") || self.model.id.starts_with("openai/"));
        let supported = self.completions_compat(
            "supportsDeveloperRole",
            openrouter_model
                || (!self.nonstandard_completions_provider()
                    && self.model.provider != "openrouter"),
        );
        if !supported {
            return;
        }
        let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
            return;
        };
        for message in messages {
            if message.get("role") == Some(&Value::String("system".to_string()))
                && let Some(message) = message.as_object_mut()
            {
                message.insert("role".to_string(), Value::String("developer".to_string()));
            }
        }
    }

    fn apply_openai_cache_control(&self, body: &mut Map<String, Value>) {
        if self
            .model
            .compat("cacheControlFormat")
            .and_then(Value::as_str)
            != Some("anthropic")
        {
            return;
        }
        let cache_control = json!({"type": "ephemeral"});
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            if let Some(message) = messages.iter_mut().find(|message| {
                matches!(
                    message.get("role").and_then(Value::as_str),
                    Some("system" | "developer")
                )
            }) {
                add_cache_control_to_message(message, &cache_control);
            }
            if let Some(message) = messages.iter_mut().rev().find(|message| {
                matches!(
                    message.get("role").and_then(Value::as_str),
                    Some("user" | "assistant" | "tool")
                )
            }) {
                add_cache_control_to_message(message, &cache_control);
            }
        }
        if let Some(tool) = body
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .and_then(|tools| tools.last_mut())
            .and_then(Value::as_object_mut)
        {
            tool.insert("cache_control".to_string(), cache_control);
        }
    }

    fn supports_reasoning_effort(&self) -> bool {
        let provider = self.model.provider.as_str();
        self.completions_compat(
            "supportsReasoningEffort",
            !matches!(
                provider,
                "xai"
                    | "zai"
                    | "zai-coding-cn"
                    | "moonshotai"
                    | "moonshotai-cn"
                    | "together"
                    | "cloudflare-ai-gateway"
                    | "nvidia"
                    | "ant-ling"
            ),
        )
    }

    fn apply_completions_thinking(&self, body: &mut Map<String, Value>) {
        if !self.model.reasoning() {
            return;
        }
        let enabled = self.thinking.enabled();
        let mapped = self.mapped_level();
        let format = self.thinking_format();
        match format {
            "zai" => {
                body.insert(
                    "thinking".to_string(),
                    if enabled {
                        json!({"type": "enabled", "clear_thinking": false})
                    } else {
                        json!({"type": "disabled"})
                    },
                );
            }
            "qwen" => {
                body.insert("enable_thinking".to_string(), Value::Bool(enabled));
            }
            "qwen-chat-template" => {
                body.insert(
                    "chat_template_kwargs".to_string(),
                    json!({"enable_thinking": enabled, "preserve_thinking": true}),
                );
            }
            "deepseek" => {
                if enabled {
                    body.insert("thinking".to_string(), json!({"type": "enabled"}));
                } else if self.off_mapping().is_some() {
                    body.insert("thinking".to_string(), json!({"type": "disabled"}));
                }
            }
            "openrouter" => {
                if enabled {
                    body.insert("reasoning".to_string(), json!({"effort": mapped}));
                } else if let Some(off) = self.off_mapping() {
                    body.insert("reasoning".to_string(), json!({"effort": off}));
                }
            }
            "together" => {
                body.insert("reasoning".to_string(), json!({"enabled": enabled}));
            }
            "ant-ling" => {
                if enabled && !mapped.is_null() {
                    body.insert("reasoning".to_string(), json!({"effort": mapped.clone()}));
                }
            }
            "string-thinking" => {
                if enabled {
                    body.insert("thinking".to_string(), mapped.clone());
                } else if let Some(off) = self.off_mapping() {
                    body.insert("thinking".to_string(), off);
                }
            }
            _ => {}
        }
        if enabled && self.supports_reasoning_effort() {
            body.insert("reasoning_effort".to_string(), mapped);
        } else if !enabled
            && self.supports_reasoning_effort()
            && let Some(off @ Value::String(_)) = self.off_mapping()
        {
            body.insert("reasoning_effort".to_string(), off);
        }
        self.apply_chat_template(body, "chatTemplateKwargs", "chat_template_kwargs");
        self.apply_chat_template(body, "chatTemplateArgs", "chat_template_args");
        let budget_field = self
            .model
            .compat("thinkingTokenBudgetField")
            .and_then(Value::as_str)
            .or_else(|| {
                self.compat_bool("supportsThinkingTokenBudget", false)
                    .then_some("thinking_token_budget")
            });
        if enabled && let Some(field) = budget_field {
            let ceiling = body
                .get("max_tokens")
                .or_else(|| body.get("max_completion_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or_else(|| self.model.max_tokens());
            let budget = self.thinking.budget().min(ceiling.saturating_sub(1_024));
            if budget > 0 {
                body.insert(field.to_string(), Value::from(budget));
            }
        }
    }

    fn apply_chat_template(&self, body: &mut Map<String, Value>, compat: &str, wire: &str) {
        let Some(values) = self.model.compat(compat).and_then(Value::as_object) else {
            return;
        };
        let mut resolved = Map::new();
        for (key, value) in values {
            let value = match value.get("$var").and_then(Value::as_str) {
                Some("thinking.enabled") => Some(Value::Bool(self.thinking.enabled())),
                Some("thinking.budget") if self.thinking.enabled() => {
                    Some(Value::from(self.thinking.budget()))
                }
                Some("thinking.effort" | "thinking.level") if self.thinking.enabled() => {
                    Some(self.mapped_level())
                }
                Some(_) => None,
                None => Some(value.clone()),
            };
            if let Some(value) = value {
                resolved.insert(key.clone(), value);
            }
        }
        if !resolved.is_empty() {
            body.insert(wire.to_string(), Value::Object(resolved));
        }
    }

    fn anthropic(&self, body: &mut Map<String, Value>) {
        self.apply_anthropic_request_compat(body);
        if !self.compat_bool("supportsTemperature", true) {
            body.remove("temperature");
        }
        if !self.model.reasoning() {
            return;
        }
        if !self.thinking.enabled() {
            if self.off_mapping().is_some() {
                body.insert("thinking".to_string(), json!({"type": "disabled"}));
            }
            return;
        }
        body.remove("temperature");
        if self.compat_bool("forceAdaptiveThinking", false) {
            body.insert(
                "thinking".to_string(),
                json!({"type": "adaptive", "display": "summarized"}),
            );
            let mapped = self.model.thinking_level(self.thinking);
            let effort = mapped.cloned().unwrap_or_else(|| {
                Value::String(
                    match self.thinking {
                        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                        ThinkingLevel::Medium => "medium",
                        _ => "high",
                    }
                    .to_string(),
                )
            });
            body.insert("output_config".to_string(), json!({"effort": effort}));
        } else {
            let ceiling = body
                .get("max_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| self.model.max_tokens());
            let budget = self.thinking.budget().min(ceiling.saturating_sub(1_024));
            body.insert(
                "thinking".to_string(),
                json!({"type": "enabled", "budget_tokens": budget, "display": "summarized"}),
            );
        }
    }

    fn apply_anthropic_request_compat(&self, body: &mut Map<String, Value>) {
        let cache_control = json!({"type": "ephemeral"});
        if let Some(system) = body.get_mut("system") {
            if let Some(text) = system.as_str() {
                *system = json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": cache_control.clone()
                }]);
            } else if let Some(blocks) = system.as_array_mut() {
                for block in blocks {
                    if let Some(block) = block.as_object_mut() {
                        block.insert("cache_control".to_string(), cache_control.clone());
                    }
                }
            }
        }
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            let eager = self.compat_bool("supportsEagerToolInputStreaming", true);
            for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
                if eager {
                    tool.insert("eager_input_streaming".to_string(), Value::Bool(true));
                } else {
                    tool.remove("eager_input_streaming");
                }
            }
            if self.compat_bool("supportsCacheControlOnTools", true)
                && let Some(tool) = tools.last_mut().and_then(Value::as_object_mut)
            {
                tool.insert("cache_control".to_string(), cache_control.clone());
            }
        }
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            let allow_empty = self.compat_bool("allowEmptySignature", false);
            for message in messages.iter_mut().filter_map(Value::as_object_mut) {
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
                    continue;
                };
                for block in blocks.iter_mut().filter_map(Value::as_object_mut) {
                    if block.get("type").and_then(Value::as_str) != Some("thinking")
                        || block
                            .get("signature")
                            .and_then(Value::as_str)
                            .is_some_and(|signature| !signature.is_empty())
                    {
                        continue;
                    }
                    if allow_empty {
                        block.insert("signature".to_string(), Value::String(String::new()));
                    } else {
                        let text = block.remove("thinking").unwrap_or_default();
                        block.clear();
                        block.insert("type".to_string(), Value::String("text".to_string()));
                        block.insert("text".to_string(), text);
                    }
                }
            }
        }
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut)
            && let Some(message) = messages
                .iter_mut()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(Value::as_object_mut)
            && let Some(content) = message.get_mut("content")
        {
            if let Some(text) = content.as_str() {
                *content = json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": cache_control
                }]);
            } else if let Some(block) = content
                .as_array_mut()
                .and_then(|blocks| blocks.last_mut())
                .and_then(Value::as_object_mut)
            {
                block.insert("cache_control".to_string(), cache_control);
            }
        }
        if let Some(fallbacks) = self
            .model
            .compat("allowedFallbackModels")
            .and_then(Value::as_array)
        {
            let fallbacks = fallbacks
                .iter()
                .filter_map(|fallback| fallback.get("model").and_then(Value::as_str))
                .map(|model| json!({"model": model}))
                .collect::<Vec<_>>();
            if !fallbacks.is_empty() {
                body.insert("fallbacks".to_string(), Value::Array(fallbacks));
            }
        }
    }

    fn google(&self, body: &mut Map<String, Value>) {
        if !self.model.reasoning() {
            return;
        }
        let config = body
            .entry("generationConfig")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(config) = config.as_object_mut() else {
            return;
        };
        let id = self.model.id.to_ascii_lowercase();
        let gemini3_pro = is_gemini3_family(&id, "pro");
        let gemini3_flash = is_gemini3_family(&id, "flash")
            || matches!(
                id.as_str(),
                "gemini-flash-latest" | "gemini-flash-lite-latest"
            );
        let gemma4 = id.contains("gemma-4") || id.contains("gemma4");
        if !self.thinking.enabled() {
            let thinking = if gemini3_pro {
                json!({"thinkingLevel": "LOW"})
            } else if gemini3_flash || gemma4 {
                json!({"thinkingLevel": "MINIMAL"})
            } else {
                json!({"thinkingBudget": 0})
            };
            config.insert("thinkingConfig".to_string(), thinking);
            return;
        }
        if gemini3_pro || gemini3_flash || gemma4 {
            let mapped = self
                .mapped_level()
                .as_str()
                .unwrap_or("high")
                .to_ascii_lowercase();
            let level = if gemini3_pro && matches!(mapped.as_str(), "minimal" | "low") {
                "low"
            } else if gemini3_pro {
                "high"
            } else {
                mapped.as_str()
            };
            config.insert(
                "thinkingConfig".to_string(),
                json!({"thinkingLevel": level.to_ascii_uppercase(), "includeThoughts": true}),
            );
        } else {
            config.insert(
                "thinkingConfig".to_string(),
                json!({"thinkingBudget": self.google_budget(), "includeThoughts": true}),
            );
        }
    }

    fn google_budget(&self) -> i64 {
        let id = self.model.id.as_str();
        let values = if id.contains("2.5-pro") {
            [128, 2_048, 8_192, 32_768]
        } else if id.contains("2.5-flash-lite") {
            [512, 2_048, 8_192, 24_576]
        } else if id.contains("2.5-flash") {
            [128, 2_048, 8_192, 24_576]
        } else {
            return -1;
        };
        match self.thinking {
            ThinkingLevel::Minimal => values[0],
            ThinkingLevel::Low => values[1],
            ThinkingLevel::Medium => values[2],
            _ => values[3],
        }
    }
}

fn is_gemini3_family(id: &str, family: &str) -> bool {
    let Some(suffix) = id.strip_prefix("gemini-3") else {
        return false;
    };
    if let Some(suffix) = suffix.strip_prefix('-') {
        return suffix.starts_with(family);
    }
    let Some(version) = suffix.strip_prefix('.') else {
        return false;
    };
    let Some((version, suffix)) = version.split_once('-') else {
        return false;
    };
    !version.is_empty()
        && version.chars().all(|character| character.is_ascii_digit())
        && suffix.starts_with(family)
}

fn add_cache_control_to_message(message: &mut Value, cache_control: &Value) {
    let Some(message) = message.as_object_mut() else {
        return;
    };
    let Some(content) = message.get_mut("content") else {
        return;
    };
    if content.as_str().is_some_and(str::is_empty) {
        return;
    }
    if let Some(text) = content.as_str() {
        *content = json!([{
            "type": "text",
            "text": text,
            "cache_control": cache_control
        }]);
        return;
    }
    let Some(parts) = content.as_array_mut() else {
        return;
    };
    if let Some(part) = parts.iter_mut().rev().find_map(|part| {
        let part = part.as_object_mut()?;
        (part.get("type").and_then(Value::as_str) == Some("text")).then_some(part)
    }) {
        part.insert("cache_control".to_string(), cache_control.clone());
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AuthClient {
    inner: reqwest::Client,
    strip_x_api_key: bool,
    transform: ModelRequestTransform,
}

impl AuthClient {
    fn prepare<T>(&self, mut request: http::Request<T>) -> http::Request<bytes::Bytes>
    where
        T: Into<bytes::Bytes>,
    {
        if self.strip_x_api_key {
            request.headers_mut().remove("x-api-key");
        }
        self.transform.transform_headers(request.headers_mut());
        let (parts, body) = request.into_parts();
        let body = self.transform.transform_bytes(body.into());
        http::Request::from_parts(parts, body)
    }
}

impl HttpClientExt for AuthClient {
    fn send<T, U>(
        &self,
        req: http::Request<T>,
    ) -> impl Future<
        Output = rig::http_client::Result<http::Response<rig::http_client::LazyBody<U>>>,
    > + Send
    + 'static
    where
        T: Into<bytes::Bytes> + Send,
        U: From<bytes::Bytes> + Send + 'static,
    {
        HttpClientExt::send(&self.inner, self.prepare(req))
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
        req: http::Request<T>,
    ) -> impl Future<Output = rig::http_client::Result<rig::http_client::StreamingResponse>> + Send
    where
        T: Into<bytes::Bytes> + Send,
    {
        HttpClientExt::send_streaming(&self.inner, self.prepare(req))
    }
}

pub(crate) struct RigBackend {
    client: RigClient,
    limits: ModelLimits,
    accepts_images: bool,
}

pub(crate) enum RigClient {
    OpenAiResponses(openai::responses_api::ResponsesCompletionModel<AuthClient>),
    OpenAiCompletions(openai::completion::CompletionModel<AuthClient>),
    Anthropic(anthropic::completion::CompletionModel<AuthClient>),
    Gemini(gemini::completion::CompletionModel<AuthClient>),
}

impl RigBackend {
    pub async fn new(
        model: &CatalogModel,
        api_key: &str,
        environment: &std::collections::BTreeMap<String, String>,
        auth_kind: AuthKind,
        thinking: ThinkingLevel,
        session_id: Option<&str>,
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
        let thinking = clamp_thinking_level(model, thinking);
        let request_client = AuthClient {
            inner: reqwest::Client::new(),
            strip_x_api_key: anthropic_oauth,
            transform: ModelRequestTransform {
                model: model.clone(),
                thinking,
                session_id: session_id.map(str::to_string),
            },
        };
        let client = match model.api.as_str() {
            "openai-responses" => RigClient::OpenAiResponses(
                openai::Client::builder()
                    .api_key(api_key)
                    .base_url(&model.base_url)
                    .http_headers(headers)
                    .http_client(request_client)
                    .build()
                    .context("cannot initialize OpenAI provider")?
                    .completion_model(&model.id),
            ),
            "openai-completions" => RigClient::OpenAiCompletions(
                openai::CompletionsClient::builder()
                    .api_key(api_key)
                    .base_url(&model.base_url)
                    .http_headers(headers)
                    .http_client(request_client)
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
                        transform: ModelRequestTransform {
                            model: model.clone(),
                            thinking,
                            session_id: session_id.map(str::to_string),
                        },
                        strip_x_api_key: anthropic_oauth,
                    });
                if anthropic_oauth {
                    builder = builder
                        .anthropic_beta("claude-code-20250219")
                        .anthropic_beta("oauth-2025-04-20");
                }
                let mut completion = builder
                    .build()
                    .context("cannot initialize Anthropic provider")?
                    .completion_model(&model.id);
                if model.compat("supportsStrictTools").and_then(Value::as_bool) == Some(true) {
                    completion = completion.with_strict_tools();
                }
                RigClient::Anthropic(completion)
            }
            "google-generative-ai" => RigClient::Gemini(
                gemini::Client::builder()
                    .api_key(api_key)
                    .base_url(normalize_gemini_base_url(&model.base_url))
                    .http_headers(headers)
                    .http_client(request_client)
                    .build()
                    .context("cannot initialize Gemini provider")?
                    .completion_model(&model.id),
            ),
            api => bail!("Pi catalog API {api:?} is not supported by the Rust backend"),
        };
        Ok(Self {
            client,
            limits,
            accepts_images: model.accepts_input("image"),
        })
    }
}

pub async fn configured_backend(
    settings: &ActiveSettings,
    catalog: &ModelCatalog,
    session_id: Option<&str>,
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
        settings.thinking,
        session_id,
    )
    .await?;
    let limits = backend.limits.clone();
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
        let mut response = match &self.client {
            RigClient::OpenAiResponses(model) => {
                complete_with(model, request, max_tokens, deltas).await
            }
            RigClient::OpenAiCompletions(model) => {
                complete_with(model, request, max_tokens, deltas).await
            }
            RigClient::Anthropic(model) => complete_with(model, request, max_tokens, deltas).await,
            RigClient::Gemini(model) => complete_with(model, request, max_tokens, deltas).await,
        }?;
        let api = match &self.client {
            RigClient::OpenAiResponses(_) => "openai-responses",
            RigClient::OpenAiCompletions(_) => "openai-completions",
            RigClient::Anthropic(_) => "anthropic-messages",
            RigClient::Gemini(_) => "google-generative-ai",
        };
        if let Some(usage) = &mut response.usage {
            normalize_usage_for_api(api, usage);
        }
        Ok(response)
    }

    fn accepts_image_input(&self) -> bool {
        self.accepts_images
    }
}

fn normalize_usage_for_api(api: &str, usage: &mut rig::completion::Usage) {
    if matches!(
        api,
        "openai-responses" | "openai-completions" | "google-generative-ai"
    ) {
        usage.input_tokens = usage
            .input_tokens
            .saturating_sub(usage.cached_input_tokens)
            .saturating_sub(usage.cache_creation_input_tokens);
    }
    if api == "google-generative-ai" {
        usage.output_tokens = usage.output_tokens.saturating_add(usage.reasoning_tokens);
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

    fn catalog_model(api: &str, metadata: Value) -> CatalogModel {
        let mut model = CatalogModel {
            id: "test-model".to_string(),
            name: "Test".to_string(),
            api: api.to_string(),
            provider: "test-provider".to_string(),
            base_url: "https://example.test/v1".to_string(),
            headers: Default::default(),
            metadata: Default::default(),
        };
        model.metadata = metadata.as_object().unwrap().clone().into_iter().collect();
        model
    }

    fn transformed(model: CatalogModel, thinking: ThinkingLevel, body: Value) -> Value {
        let bytes = serde_json::to_vec(&body).unwrap();
        serde_json::from_slice(
            &ModelRequestTransform {
                model,
                thinking,
                session_id: None,
            }
            .transform_bytes(bytes::Bytes::from(bytes)),
        )
        .unwrap()
    }

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
    fn provider_usage_is_normalized_before_catalog_pricing() {
        let mut openai = rig::completion::Usage {
            input_tokens: 1_000,
            output_tokens: 200,
            cached_input_tokens: 300,
            cache_creation_input_tokens: 100,
            ..rig::completion::Usage::new()
        };
        normalize_usage_for_api("openai-responses", &mut openai);
        assert_eq!(openai.input_tokens, 600);
        assert_eq!(openai.output_tokens, 200);

        let mut gemini = rig::completion::Usage {
            input_tokens: 1_000,
            output_tokens: 200,
            cached_input_tokens: 300,
            reasoning_tokens: 50,
            ..rig::completion::Usage::new()
        };
        normalize_usage_for_api("google-generative-ai", &mut gemini);
        assert_eq!(gemini.input_tokens, 700);
        assert_eq!(gemini.output_tokens, 250);

        let mut anthropic = rig::completion::Usage {
            input_tokens: 600,
            output_tokens: 200,
            cached_input_tokens: 300,
            cache_creation_input_tokens: 100,
            ..rig::completion::Usage::new()
        };
        normalize_usage_for_api("anthropic-messages", &mut anthropic);
        assert_eq!(anthropic.input_tokens, 600);
        assert_eq!(anthropic.output_tokens, 200);
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

    #[test]
    fn responses_uses_catalog_thinking_map_and_sampling_params() {
        let model = catalog_model(
            "openai-responses",
            json!({
                "reasoning": true,
                "thinkingLevelMap": {"high": "xhigh"},
                "samplingParams": {"service_tier": "priority"}
            }),
        );
        let body = transformed(
            model,
            ThinkingLevel::High,
            json!({"model": "test-model", "stream": true}),
        );
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn completions_compat_renames_output_cap_and_controls_reasoning() {
        let model = catalog_model(
            "openai-completions",
            json!({
                "reasoning": true,
                "thinkingLevelMap": {"off": null, "low": "small"},
                "compat": {
                    "maxTokensField": "max_tokens",
                    "supportsReasoningEffort": true,
                    "supportsUsageInStreaming": false
                }
            }),
        );
        let body = transformed(
            model,
            ThinkingLevel::Low,
            json!({
                "max_completion_tokens": 4096,
                "stream_options": {"include_usage": true}
            }),
        );
        assert_eq!(body["max_tokens"], 4096);
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("stream_options").is_none());
        assert_eq!(body["reasoning_effort"], "small");
    }

    #[test]
    fn completions_cache_control_and_session_affinity_match_pi() {
        let model = catalog_model(
            "openai-completions",
            json!({
                "compat": {
                    "cacheControlFormat": "anthropic",
                    "sendSessionAffinityHeaders": true
                }
            }),
        );
        let transform = ModelRequestTransform {
            model: model.clone(),
            thinking: ThinkingLevel::Off,
            session_id: Some("session-1".to_string()),
        };
        let mut headers = HeaderMap::new();
        transform.transform_headers(&mut headers);
        assert_eq!(headers["session_id"], "session-1");
        assert_eq!(headers["x-client-request-id"], "session-1");
        assert_eq!(headers["x-session-affinity"], "session-1");

        let body = transformed(
            model,
            ThinkingLevel::Off,
            json!({
                "messages": [
                    {"role": "system", "content": "instructions"},
                    {"role": "user", "content": "hello"}
                ],
                "tools": [{"type": "function", "function": {"name": "read"}}]
            }),
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn anthropic_affinity_uses_only_its_documented_header() {
        let model = catalog_model(
            "anthropic-messages",
            json!({"compat": {"sendSessionAffinityHeaders": true}}),
        );
        let transform = ModelRequestTransform {
            model,
            thinking: ThinkingLevel::Off,
            session_id: Some("session-1".to_string()),
        };
        let mut headers = HeaderMap::new();
        transform.transform_headers(&mut headers);
        assert_eq!(headers["x-session-affinity"], "session-1");
        assert!(!headers.contains_key("session_id"));
        assert!(!headers.contains_key("x-client-request-id"));
    }

    #[tokio::test]
    async fn anthropic_strict_tools_follow_catalog_compat() {
        for (supports, expected) in [(false, false), (true, true)] {
            let model = catalog_model(
                "anthropic-messages",
                json!({"compat": {"supportsStrictTools": supports}}),
            );
            let backend = RigBackend::new(
                &model,
                "test-key",
                &Default::default(),
                AuthKind::ApiKey,
                ThinkingLevel::Off,
                None,
            )
            .await
            .unwrap();
            assert!(matches!(
                backend.client,
                RigClient::Anthropic(client) if client.strict_tools == expected
            ));
        }
    }

    #[test]
    fn anthropic_force_adaptive_thinking_matches_pi_request_shape() {
        let model = catalog_model(
            "anthropic-messages",
            json!({
                "reasoning": true,
                "thinkingLevelMap": {"xhigh": "max"},
                "compat": {"forceAdaptiveThinking": true}
            }),
        );
        let body = transformed(
            model,
            ThinkingLevel::Xhigh,
            json!({"max_tokens": 16384, "temperature": 0.4}),
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["output_config"]["effort"], "max");
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn gemini_uses_level_for_v3_and_budget_for_v25() {
        let mut v3 = catalog_model("google-generative-ai", json!({"reasoning": true}));
        v3.id = "gemini-3-flash".to_string();
        let v3_body = transformed(v3, ThinkingLevel::Medium, json!({}));
        assert_eq!(
            v3_body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "MEDIUM"
        );
        assert_eq!(
            v3_body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );

        let mut v25 = catalog_model("google-generative-ai", json!({"reasoning": true}));
        v25.id = "gemini-2.5-pro".to_string();
        let v25_body = transformed(v25, ThinkingLevel::High, json!({}));
        assert_eq!(
            v25_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            32_768
        );

        let mut v31 = catalog_model(
            "google-generative-ai",
            json!({
                "reasoning": true,
                "thinkingLevelMap": {"off": null, "low": "LOW", "high": "HIGH"}
            }),
        );
        v31.id = "gemini-3.1-pro-preview".to_string();
        let v31_body = transformed(v31, ThinkingLevel::Low, json!({}));
        assert_eq!(
            v31_body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "LOW"
        );

        let mut flash = catalog_model("google-generative-ai", json!({"reasoning": true}));
        flash.id = "gemini-2.5-flash".to_string();
        let flash_body = transformed(flash, ThinkingLevel::Minimal, json!({}));
        assert_eq!(
            flash_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            128
        );

        let mut automatic = catalog_model("google-generative-ai", json!({"reasoning": true}));
        automatic.id = "deep-research-preview-04-2026".to_string();
        let automatic_body = transformed(automatic, ThinkingLevel::High, json!({}));
        assert_eq!(
            automatic_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            -1
        );
    }
}
