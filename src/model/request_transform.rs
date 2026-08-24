use crate::catalog::{CatalogModel, ThinkingLevel};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

#[derive(Clone, Debug)]
pub(super) struct ModelRequestTransform {
    pub(super) model: CatalogModel,
    pub(super) thinking: ThinkingLevel,
    pub(super) session_id: Option<String>,
}

impl ModelRequestTransform {
    pub(super) fn transform_bytes(&self, bytes: bytes::Bytes) -> bytes::Bytes {
        let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
            return bytes;
        };
        let Some(body) = value.as_object_mut() else {
            return bytes;
        };
        match self.model.api.as_str() {
            "openai-responses" => self.openai_responses(body),
            "openai-codex-responses" => self.openai_codex_responses(body),
            "openai-completions" => self.openai_completions(body),
            "anthropic-messages" => self.anthropic(body),
            "google-generative-ai" => self.google(body),
            _ => {}
        }
        serde_json::to_vec(&value).map_or(bytes, bytes::Bytes::from)
    }

    pub(super) fn transform_headers(&self, headers: &mut HeaderMap) {
        self.apply_session_affinity(headers);
        if self.model.api == "openai-codex-responses" {
            headers.insert(
                HeaderName::from_static("openai-beta"),
                HeaderValue::from_static("responses=experimental"),
            );
        }
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
        if self.model.api == "openai-codex-responses" {
            headers.remove("session_id");
            let Some(session_id) = self.codex_session_id() else {
                return;
            };
            let insert = |headers: &mut HeaderMap, name: &'static str| {
                if let Ok(value) = HeaderValue::from_str(&session_id) {
                    headers.insert(HeaderName::from_static(name), value);
                }
            };
            insert(headers, "session-id");
            insert(headers, "x-client-request-id");
            return;
        }
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

    fn codex_session_id(&self) -> Option<String> {
        self.session_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| value.chars().take(64).collect())
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

    fn apply_tool_strictness(
        &self,
        body: &mut Map<String, Value>,
        supported_default: bool,
        value: Value,
    ) {
        let supported = self.compat_bool("supportsStrictMode", supported_default);
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
                function.entry("strict").or_insert_with(|| value.clone());
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
        let (strict_supported, strict_value) = if self.model.api == "openai-codex-responses" {
            (true, Value::Null)
        } else {
            (false, Value::Bool(false))
        };
        self.apply_tool_strictness(body, strict_supported, strict_value);
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

    fn openai_codex_responses(&self, body: &mut Map<String, Value>) {
        self.openai_responses(body);
        if !self.thinking.enabled() {
            body.remove("reasoning");
        }
        body.insert("stream".to_string(), Value::Bool(true));
        body.insert(
            "include".to_string(),
            json!(["reasoning.encrypted_content"]),
        );
        body.insert("tool_choice".to_string(), Value::String("auto".to_string()));
        body.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        body.insert("text".to_string(), json!({"verbosity": "low"}));
        if let Some(session_id) = self.codex_session_id() {
            body.insert("prompt_cache_key".to_string(), Value::String(session_id));
        }
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
        self.apply_tool_strictness(body, strict_default, Value::Bool(false));
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
