use super::antigravity::AntigravityTransport;
use super::codex_websocket::CodexWebSocketTransport;
use super::failure::{ModelFailure, ModelFailurePhase};
use super::request_transform::ModelRequestTransform;
use super::{ModelBackend, ModelDelta, ModelRequest, ModelResponse, clamp_thinking_level};
use crate::catalog::{CatalogModel, ModelCatalog, ModelLimits, ThinkingLevel};
use crate::config::{ActiveSettings, AuthKind, ConfigManager, resolve_config_value};
use crate::oauth;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use rig::client::CompletionClient;
use rig::completion::CompletionModel as RigCompletionModel;
use rig::http_client::HttpClientExt;
use rig::message::AssistantContent;
use rig::providers::{anthropic, chatgpt, gemini, openai};
use rig::streaming::StreamedAssistantContent;
use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

pub(super) fn has_usable_assistant_content(content: &[AssistantContent]) -> bool {
    content.iter().any(|content| match content {
        AssistantContent::Text(text) => !text.text.trim().is_empty(),
        AssistantContent::ToolCall(_) => true,
        AssistantContent::Reasoning(_) | AssistantContent::Image(_) => false,
    })
}

const CONTEXT_SAFETY_TOKENS: usize = 4_096;

pub(super) fn clamp_max_tokens_to_context(limits: &ModelLimits, estimated_context: usize) -> u64 {
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

#[derive(Clone, Debug, Default)]
pub(crate) struct AuthClient {
    pub(super) inner: reqwest::Client,
    pub(super) strip_x_api_key: bool,
    pub(super) transform: Option<ModelRequestTransform>,
    pub(super) codex_websocket: Option<CodexWebSocketTransport>,
    pub(super) antigravity: Option<AntigravityTransport>,
}

impl AuthClient {
    pub(super) fn prepare<T>(&self, mut request: http::Request<T>) -> http::Request<bytes::Bytes>
    where
        T: Into<bytes::Bytes>,
    {
        if self.strip_x_api_key {
            request.headers_mut().remove("x-api-key");
        }
        let (mut parts, body) = request.into_parts();
        let body = if let Some(transform) = &self.transform {
            transform.transform_headers(&mut parts.headers);
            transform.transform_bytes(body.into())
        } else {
            body.into()
        };
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
        let inner = self.inner.clone();
        let request = self.prepare(req);
        let codex_websocket = self.codex_websocket.clone();
        let antigravity = self.antigravity.clone();
        async move {
            if let Some(antigravity) = antigravity {
                antigravity.send(inner, request).await
            } else if let Some(codex_websocket) = codex_websocket {
                codex_websocket.send(inner, request).await
            } else {
                HttpClientExt::send_streaming(&inner, request).await
            }
        }
    }
}

pub(crate) struct RigBackend {
    pub(super) client: RigClient,
    pub(super) limits: ModelLimits,
    pub(super) accepts_images: bool,
}

pub(crate) enum RigClient {
    OpenAiResponses(openai::responses_api::ResponsesCompletionModel<AuthClient>),
    OpenAiCodexResponses(chatgpt::ResponsesCompletionModel<AuthClient>),
    OpenAiCompletions(openai::completion::CompletionModel<AuthClient>),
    Anthropic(anthropic::completion::CompletionModel<AuthClient>),
    Gemini(gemini::completion::CompletionModel<AuthClient>),
}

struct DeferredRigBackend {
    model: CatalogModel,
    settings: ActiveSettings,
    session_id: Option<String>,
    manager: Arc<ConfigManager>,
    backend: Mutex<Option<(String, Arc<RigBackend>)>>,
}

impl DeferredRigBackend {
    async fn backend(&self) -> Result<Arc<RigBackend>> {
        let api_key = self
            .manager
            .resolve_model_api_key(&self.settings)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no credential configured; press :login"))?;
        let mut cached = self.backend.lock().await;
        if let Some((cached_api_key, backend)) = cached.as_ref()
            && cached_api_key == &api_key
        {
            return Ok(backend.clone());
        }
        let backend = Arc::new(
            RigBackend::new_with_manager(
                &self.model,
                &api_key,
                &self.settings.credential_environment,
                self.settings.auth_kind,
                self.settings.thinking,
                self.session_id.as_deref(),
                Some(self.manager.clone()),
            )
            .await?,
        );
        *cached = Some((api_key, backend.clone()));
        Ok(backend)
    }
}

impl RigBackend {
    #[cfg(test)]
    pub async fn new(
        model: &CatalogModel,
        api_key: &str,
        environment: &std::collections::BTreeMap<String, String>,
        auth_kind: AuthKind,
        thinking: ThinkingLevel,
        session_id: Option<&str>,
    ) -> Result<Self> {
        Self::new_with_manager(
            model,
            api_key,
            environment,
            auth_kind,
            thinking,
            session_id,
            None,
        )
        .await
    }

    async fn new_with_manager(
        model: &CatalogModel,
        api_key: &str,
        environment: &std::collections::BTreeMap<String, String>,
        auth_kind: AuthKind,
        thinking: ThinkingLevel,
        session_id: Option<&str>,
        manager: Option<Arc<ConfigManager>>,
    ) -> Result<Self> {
        if model.api == "openai-codex-responses" {
            if model.provider != "openai-codex" {
                bail!("openai-codex-responses is only supported for the openai-codex provider");
            }
            if auth_kind != AuthKind::Oauth {
                bail!(
                    "openai-codex-responses requires ChatGPT/Codex subscription OAuth; run :login and select OpenAI"
                );
            }
        }
        if model.api == "antigravity" && auth_kind != AuthKind::Oauth {
            bail!("Antigravity requires Google OAuth; run :login and select Antigravity");
        }
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
        let codex_account_id = (model.api == "openai-codex-responses")
            .then(|| oauth::chatgpt_account_id(api_key))
            .transpose()
            .context("invalid ChatGPT/Codex OAuth access token")?;
        let antigravity = (model.api == "antigravity")
            .then(|| {
                AntigravityTransport::new(
                    model,
                    thinking,
                    session_id,
                    manager.ok_or_else(|| {
                        anyhow::anyhow!("Antigravity credential store is unavailable")
                    })?,
                )
            })
            .transpose()?;
        let request_client = AuthClient {
            inner: reqwest::Client::new(),
            strip_x_api_key: anthropic_oauth,
            transform: Some(ModelRequestTransform {
                model: model.clone(),
                thinking,
                session_id: session_id.map(str::to_string),
            }),
            codex_websocket: (model.api == "openai-codex-responses")
                .then(|| CodexWebSocketTransport::new(session_id.zip(codex_account_id.as_deref()))),
            antigravity,
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
            "openai-codex-responses" => {
                let account_id = codex_account_id.expect("Codex account ID validated above");
                RigClient::OpenAiCodexResponses(
                    chatgpt::Client::builder()
                        .api_key(chatgpt::ChatGPTAuth::AccessToken {
                            access_token: api_key.to_string(),
                            account_id: Some(account_id),
                        })
                        .base_url(normalize_chatgpt_codex_base_url(&model.base_url))
                        .http_headers(headers)
                        .http_client(request_client)
                        .default_instructions("")
                        .originator("pi")
                        .user_agent(format!(
                            "uri-agent/{} ({} {}; pi)",
                            env!("CARGO_PKG_VERSION"),
                            std::env::consts::OS,
                            std::env::consts::ARCH
                        ))
                        .build()
                        .context("cannot initialize ChatGPT/Codex provider")?
                        .completion_model(&model.id),
                )
            }
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
                        transform: Some(ModelRequestTransform {
                            model: model.clone(),
                            thinking,
                            session_id: session_id.map(str::to_string),
                        }),
                        codex_websocket: None,
                        antigravity: None,
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
            "google-generative-ai" | "antigravity" => RigClient::Gemini(
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
    manager: Arc<ConfigManager>,
) -> Result<Option<(Arc<dyn ModelBackend>, ModelLimits)>> {
    if !settings.model_configured() {
        return Ok(None);
    }
    if settings.api_key.is_none() {
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
    let backend = DeferredRigBackend {
        model,
        settings: settings.clone(),
        session_id: session_id.map(str::to_string),
        manager,
        backend: Mutex::new(None),
    };
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

pub(super) fn normalize_chatgpt_codex_base_url(base_url: &str) -> String {
    let base_url = base_url.trim().trim_end_matches('/');
    let base_url = if base_url.is_empty() {
        "https://chatgpt.com/backend-api"
    } else {
        base_url
    };
    if let Some(base_url) = base_url.strip_suffix("/codex/responses") {
        return format!("{base_url}/codex");
    }
    if base_url.ends_with("/codex") {
        base_url.to_string()
    } else {
        format!("{base_url}/codex")
    }
}

#[async_trait]
impl ModelBackend for RigBackend {
    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse> {
        let max_tokens = clamp_max_tokens_to_context(&self.limits, request.estimated_context).min(
            request
                .max_output_tokens
                .and_then(|tokens| u64::try_from(tokens).ok())
                .unwrap_or(u64::MAX),
        );
        let mut response = match &self.client {
            RigClient::OpenAiResponses(model) => {
                complete_with(model, request, max_tokens, deltas).await
            }
            RigClient::OpenAiCodexResponses(model) => {
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
            RigClient::OpenAiCodexResponses(_) => "openai-codex-responses",
            RigClient::OpenAiCompletions(_) => "openai-completions",
            RigClient::Anthropic(_) => "anthropic-messages",
            RigClient::Gemini(_) => "google-generative-ai",
        };
        response.context_tokens = response
            .usage
            .as_ref()
            .and_then(|usage| (usage.total_tokens > 0).then_some(usage.total_tokens as usize));
        if let Some(usage) = &mut response.usage {
            normalize_usage_for_api(api, usage);
        }
        Ok(response)
    }

    fn accepts_image_input(&self) -> bool {
        self.accepts_images
    }

    fn desired_max_output_tokens(&self) -> usize {
        self.limits.max_tokens as usize
    }
}

#[async_trait]
impl ModelBackend for DeferredRigBackend {
    async fn prepare(&self) -> Result<()> {
        self.backend().await.map(|_| ())
    }

    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse> {
        self.backend().await?.complete(request, deltas).await
    }

    fn accepts_image_input(&self) -> bool {
        self.model.accepts_input("image")
    }

    fn desired_max_output_tokens(&self) -> usize {
        self.model.max_tokens() as usize
    }
}

pub(super) fn normalize_usage_for_api(api: &str, usage: &mut rig::completion::Usage) {
    if matches!(
        api,
        "openai-responses"
            | "openai-codex-responses"
            | "openai-completions"
            | "google-generative-ai"
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
    if !request.tools.is_empty() {
        completion = completion.tools(request.tools);
    }
    let mut stream = completion
        .stream()
        .await
        .map_err(|error| ModelFailure::from_completion_error(error, ModelFailurePhase::Request))?;
    let mut reasoning_deltas = HashSet::new();
    while let Some(event) = stream.next().await {
        match event.map_err(|error| {
            ModelFailure::from_completion_error(error, ModelFailurePhase::Stream)
        })? {
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
    if !has_usable_assistant_content(&stream.choice) {
        return Err(ModelFailure::empty_response().into());
    }
    Ok(ModelResponse {
        content: stream.choice.clone(),
        usage: stream.response.as_ref().map(|final_| final_.usage),
        context_tokens: None,
        finish_reason: stream
            .response
            .as_ref()
            .and_then(|final_| final_.finish_reason.clone()),
    })
}

#[cfg(test)]
mod deferred_tests {
    use super::*;
    use crate::config::ValueSource;
    use crate::oauth::OauthToken;

    #[tokio::test]
    async fn rebuilds_after_kimi_oauth_credential_rotates() {
        let root = tempfile::tempdir().unwrap();
        let manager =
            ConfigManager::load_for_test(&root.path().join("config"), &root.path().join("project"))
                .await
                .unwrap();
        let stale = OauthToken {
            kind: "oauth".to_string(),
            refresh: "stale-refresh".to_string(),
            access: "stale-access".to_string(),
            expires: i64::MAX,
            extra: Default::default(),
        };
        manager
            .set_oauth("kimi-coding", stale.clone())
            .await
            .unwrap();
        let mut settings = manager.current().await;
        settings.provider = "kimi-coding".to_string();
        settings.model = "test-model".to_string();
        settings.api_key = Some(stale.access.clone());
        settings.auth_kind = AuthKind::Oauth;
        settings.api_key_source = ValueSource::Global;
        let deferred = DeferredRigBackend {
            model: CatalogModel {
                id: "test-model".to_string(),
                name: "Test".to_string(),
                api: "anthropic-messages".to_string(),
                provider: "kimi-coding".to_string(),
                base_url: "https://example.test".to_string(),
                headers: Default::default(),
                metadata: Default::default(),
            },
            settings,
            session_id: None,
            manager: manager.clone(),
            backend: Mutex::new(None),
        };
        let initial_backend = deferred.backend().await.unwrap();
        manager
            .set_oauth(
                "kimi-coding",
                OauthToken {
                    kind: "oauth".to_string(),
                    refresh: "rotated-refresh".to_string(),
                    access: "fresh-access".to_string(),
                    expires: i64::MAX,
                    extra: Default::default(),
                },
            )
            .await
            .unwrap();
        let refreshed_backend = deferred.backend().await.unwrap();
        let reused_backend = deferred.backend().await.unwrap();

        assert!(!Arc::ptr_eq(&initial_backend, &refreshed_backend));
        assert!(Arc::ptr_eq(&refreshed_backend, &reused_backend));
        let stored = manager.oauth_token("kimi-coding").await.unwrap();
        assert_eq!(stored.access, "fresh-access");
        assert_eq!(stored.refresh, "rotated-refresh");
    }
}
