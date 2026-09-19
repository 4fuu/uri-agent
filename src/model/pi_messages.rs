//! Native implementation of pi's message and event protocol.

use super::{ModelBackend, ModelDelta, ModelFailure, ModelRequest, ModelResponse};
use crate::catalog::{CatalogModel, ThinkingLevel};
use crate::config::{ActiveSettings, AuthKind, ConfigManager, ValueSource};
use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode, Url};
use rig::completion::{CompletionError, FinishReason, Usage};
use rig::message::{
    AssistantContent, DocumentSourceKind, Message, MimeType, Reasoning, ReasoningContent, ToolCall,
    ToolFunction, ToolResultContent, UserContent,
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;

const RADIUS_PROVIDER: &str = "radius";
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

fn validate_endpoint_url(url: &Url) -> Result<()> {
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("pi-messages URL must not contain credentials, query, or fragment");
    }
    let secure = url.scheme() == "https";
    let loopback = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if !secure && !loopback {
        bail!("pi-messages URL must use HTTPS (HTTP is allowed only for loopback tests)");
    }
    Ok(())
}

fn origin(url: &Url) -> (&str, Option<&str>, Option<u16>) {
    (url.scheme(), url.host_str(), url.port_or_known_default())
}

pub(super) struct PiMessagesBackend {
    model: CatalogModel,
    settings: ActiveSettings,
    session_id: Option<String>,
    manager: Arc<ConfigManager>,
    client: Client,
    endpoint: Url,
}

impl PiMessagesBackend {
    pub(super) fn new(
        model: CatalogModel,
        settings: ActiveSettings,
        session_id: Option<&str>,
        manager: Arc<ConfigManager>,
    ) -> Result<Self> {
        if model.api != "pi-messages" {
            bail!("pi-messages backend cannot run catalog API {}", model.api);
        }
        let base = Url::parse(&model.base_url).context("invalid pi-messages base URL")?;
        validate_endpoint_url(&base)?;
        let endpoint = base
            .join(&format!("{}/messages", base.path().trim_end_matches('/')))
            .context("invalid pi-messages endpoint")?;
        Ok(Self {
            model,
            settings,
            session_id: session_id.map(str::to_owned),
            manager,
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .pool_idle_timeout(Duration::from_secs(90))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            endpoint,
        })
    }

    async fn token(&self) -> Result<String> {
        self.manager
            .resolve_model_api_key(&self.settings)
            .await?
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| anyhow!("{} requires an API key or login", self.model.provider))
    }

    fn refreshable(&self) -> bool {
        self.model.provider == RADIUS_PROVIDER
            && self.settings.auth_kind == AuthKind::Oauth
            && self.settings.api_key_source == ValueSource::Global
    }

    async fn attempt(
        &self,
        request: &ModelRequest,
        deltas: &mpsc::UnboundedSender<ModelDelta>,
        token: &str,
    ) -> std::result::Result<ModelResponse, AttemptError> {
        if self.model.provider == RADIUS_PROVIDER {
            let gateway = self.manager.radius_gateway().await;
            let gateway = Url::parse(&gateway)
                .context("invalid stored Radius gateway")
                .map_err(AttemptError::Setup)?;
            validate_endpoint_url(&gateway).map_err(AttemptError::Setup)?;
            if origin(&self.endpoint) != origin(&gateway) {
                return Err(AttemptError::Setup(anyhow!(
                    "Radius model endpoint and OAuth gateway must have the same origin"
                )));
            }
        }
        let payload = request_payload(
            &self.model,
            request,
            self.settings.thinking,
            self.session_id.as_deref(),
        )
        .map_err(AttemptError::Setup)?;
        let mut builder = self
            .client
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(ACCEPT, "text/event-stream")
            .header(CONTENT_TYPE, "application/json");
        if self.model.provider != RADIUS_PROVIDER {
            for (name, value) in &self.model.headers {
                builder = builder.header(name, value);
            }
        }
        let response = builder
            .json(&payload)
            .send()
            .await
            .map_err(AttemptError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = response.text().await.unwrap_or_default();
            return Err(AttemptError::Request(status, headers, body));
        }
        collect_stream(response, deltas)
            .await
            .map_err(AttemptError::Stream)
    }
}

#[async_trait]
impl ModelBackend for PiMessagesBackend {
    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse> {
        let token = self.token().await?;
        let response = match self.attempt(&request, &deltas, &token).await {
            Err(AttemptError::Request(StatusCode::UNAUTHORIZED, _, _)) if self.refreshable() => {
                let fresh = self
                    .manager
                    .force_refresh_oauth(RADIUS_PROVIDER)
                    .await?
                    .access;
                self.attempt(&request, &deltas, &fresh)
                    .await
                    .map_err(|error| error.failure(&self.model.provider))
            }
            result => result.map_err(|error| error.failure(&self.model.provider)),
        }?;
        if !super::rig_backend::has_usable_assistant_content(&response.content) {
            return Err(ModelFailure::empty_response().into());
        }
        Ok(response)
    }

    fn accepts_image_input(&self) -> bool {
        self.model.accepts_input("image")
    }

    fn desired_max_output_tokens(&self) -> usize {
        self.model.max_tokens() as usize
    }
}

enum AttemptError {
    Setup(anyhow::Error),
    Transport(reqwest::Error),
    Request(StatusCode, http::HeaderMap, String),
    Stream(String),
}

impl AttemptError {
    fn failure(self, provider: &str) -> anyhow::Error {
        match self {
            Self::Setup(error) => error,
            Self::Transport(error) => ModelFailure::from_completion_error(
                CompletionError::RequestError(Box::new(error)),
                super::ModelFailurePhase::Request,
                provider,
            )
            .into(),
            Self::Request(status, headers, body) => ModelFailure::from_completion_error(
                CompletionError::ProviderResponse(
                    rig::ProviderResponseError::new(status, body)
                        .with_headers(Some(Box::new(headers))),
                ),
                super::ModelFailurePhase::Request,
                provider,
            )
            .into(),
            Self::Stream(message) => ModelFailure::from_completion_error(
                CompletionError::ResponseError(message),
                super::ModelFailurePhase::Stream,
                provider,
            )
            .into(),
        }
    }
}

fn request_payload(
    model: &CatalogModel,
    request: &ModelRequest,
    thinking: ThinkingLevel,
    session_id: Option<&str>,
) -> Result<Value> {
    let messages = request
        .history
        .iter()
        .map(|message| message_to_pi(message, model))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let tools = request
        .tools
        .iter()
        .map(|tool| json!({"name":tool.name,"description":tool.description,"parameters":tool.parameters}))
        .collect::<Vec<_>>();
    let mut options = Map::new();
    let available =
        super::rig_backend::clamp_max_tokens_to_context(&model.limits(), request.estimated_context);
    let requested = request
        .max_output_tokens
        .unwrap_or(model.max_tokens() as usize);
    options.insert(
        "maxTokens".into(),
        requested.min(available as usize).max(1).into(),
    );
    let thinking = super::clamp_thinking_level(model, thinking);
    if model.reasoning() && thinking.enabled() {
        options.insert("reasoning".into(), thinking.as_str().into());
    }
    if let Some(id) = session_id.filter(|id| !id.is_empty()) {
        options.insert("sessionId".into(), id.into());
    }
    Ok(json!({
        "model": model.id,
        "context": {"systemPrompt":request.system,"messages":messages,"tools":tools},
        "options": options
    }))
}

fn message_to_pi(message: &Message, model: &CatalogModel) -> Result<Vec<Value>> {
    match message {
        Message::System { content } => {
            Ok(vec![json!({"role":"user","content":content,"timestamp":0})])
        }
        Message::User { content } => {
            let mut messages = Vec::new();
            let mut blocks = Vec::new();
            for item in content {
                if let UserContent::ToolResult(result) = item {
                    if !blocks.is_empty() {
                        messages.push(json!({"role":"user","content":std::mem::take(&mut blocks),"timestamp":0}));
                    }
                    let result_blocks = result
                        .content
                        .iter()
                        .map(tool_result_content)
                        .collect::<Result<Vec<_>>>()?;
                    messages.push(json!({"role":"toolResult","toolCallId":result.wire_call_id(),"toolName":result.name,"content":result_blocks,"isError":false,"timestamp":0}));
                } else {
                    blocks.push(user_content(item)?);
                }
            }
            if !blocks.is_empty() {
                messages.push(json!({"role":"user","content":blocks,"timestamp":0}));
            }
            Ok(messages)
        }
        Message::Assistant { id, content } => {
            let blocks = content
                .iter()
                .map(assistant_content)
                .collect::<Result<Vec<_>>>()?;
            let stop_reason = if content
                .iter()
                .any(|item| matches!(item, AssistantContent::ToolCall(_)))
            {
                "toolUse"
            } else {
                "stop"
            };
            let mut value = json!({"role":"assistant","content":blocks,"api":"pi-messages","provider":model.provider,"model":model.id,"usage":empty_pi_usage(),"stopReason":stop_reason,"timestamp":0});
            if let Some(id) = id {
                value["responseId"] = id.clone().into();
            }
            Ok(vec![value])
        }
    }
}

fn user_content(content: &UserContent) -> Result<Value> {
    match content {
        UserContent::Text(text) => Ok(json!({"type":"text","text":text.text})),
        UserContent::Image(image) => image_content(image),
        UserContent::ToolResult(_) => bail!("tool results must be emitted as correlated messages"),
        _ => bail!("pi-messages cannot represent this user content type"),
    }
}

fn tool_result_content(content: &ToolResultContent) -> Result<Value> {
    match content {
        ToolResultContent::Text(text) => Ok(json!({"type":"text","text":text.text})),
        ToolResultContent::Image(image) => image_content(image),
        ToolResultContent::Json { value } => Ok(json!({"type":"text","text":value.to_string()})),
    }
}

fn image_content(image: &rig::message::Image) -> Result<Value> {
    let data = match &image.data {
        DocumentSourceKind::Base64(data) => data.clone(),
        DocumentSourceKind::Raw(data) => {
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data)
        }
        _ => bail!("pi-messages only supports inline base64 image history"),
    };
    let mime = image
        .media_type
        .as_ref()
        .map(|kind| kind.to_mime_type().to_owned())
        .unwrap_or_else(|| "image/png".into());
    Ok(json!({"type":"image","data":data,"mimeType":mime}))
}

fn assistant_content(content: &AssistantContent) -> Result<Value> {
    match content {
        AssistantContent::Text(text) => {
            let signature = text
                .additional_params
                .as_ref()
                .and_then(|params| params.get("textSignature"))
                .cloned();
            Ok(json!({"type":"text","text":text.text,"textSignature":signature}))
        }
        AssistantContent::Reasoning(reasoning) => match reasoning.content.as_slice() {
            [ReasoningContent::Redacted { data }] => Ok(
                json!({"type":"thinking","thinking":"","thinkingSignature":data,"redacted":true}),
            ),
            _ => Ok(
                json!({"type":"thinking","thinking":reasoning.display_text(),"thinkingSignature":reasoning.first_signature().or_else(|| reasoning.encrypted_content())}),
            ),
        },
        AssistantContent::ToolCall(call) => {
            let mut value = call
                .additional_params
                .as_ref()
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            value.insert("type".into(), "toolCall".into());
            value.insert("id".into(), call.wire_call_id().into());
            value.insert("name".into(), call.function.name.clone().into());
            value.insert("arguments".into(), call.function.arguments.clone());
            value.insert("thoughtSignature".into(), call.signature.clone().into());
            Ok(Value::Object(value))
        }
        AssistantContent::Image(_) => {
            bail!("pi-messages history cannot represent assistant images")
        }
    }
}

fn empty_pi_usage() -> Value {
    json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}})
}

async fn collect_stream(
    response: reqwest::Response,
    deltas: &mpsc::UnboundedSender<ModelDelta>,
) -> std::result::Result<ModelResponse, String> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut content = BTreeMap::new();
    loop {
        let chunk = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| "pi-messages stream idle timeout".to_owned())?;
        let Some(chunk) = chunk else { break };
        buffer.extend_from_slice(&chunk.map_err(|error| error.to_string())?);
        if buffer.len() > MAX_EVENT_BYTES && event_boundary(&buffer).is_none() {
            return Err("pi-messages SSE event exceeds size limit".into());
        }
        while let Some((split, delimiter_len)) = event_boundary(&buffer) {
            let raw = buffer.drain(..split + delimiter_len).collect::<Vec<_>>();
            if let Some(value) = parse_event(&raw[..split])?
                && let Some(done) = apply_event(value, &mut content, deltas)?
            {
                return done;
            }
        }
    }
    if !buffer.iter().all(u8::is_ascii_whitespace)
        && let Some(value) = parse_event(&buffer)?
        && let Some(done) = apply_event(value, &mut content, deltas)?
    {
        return done;
    }
    Err("pi-messages stream ended without a terminal event".into())
}

fn event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer
        .windows(2)
        .position(|part| part == b"\n\n")
        .map(|i| (i, 2));
    let crlf = buffer
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

fn parse_event(raw: &[u8]) -> std::result::Result<Option<Value>, String> {
    let text = std::str::from_utf8(raw)
        .map_err(|e| format!("invalid SSE UTF-8: {e}"))?
        .replace("\r\n", "\n");
    let data = text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .map(|value| value.strip_prefix(' ').unwrap_or(value))
        })
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    if data == "[DONE]" {
        return Ok(None);
    }
    serde_json::from_str(&data)
        .map(Some)
        .map_err(|e| format!("invalid pi-messages event: {e}"))
}

#[derive(Default)]
enum StreamContent {
    #[default]
    Empty,
    Text(String),
    Thinking(String),
    Tool(String),
    Final(Box<AssistantContent>),
}

fn apply_event(
    value: Value,
    content: &mut BTreeMap<u64, StreamContent>,
    deltas: &mpsc::UnboundedSender<ModelDelta>,
) -> std::result::Result<Option<std::result::Result<ModelResponse, String>>, String> {
    let kind = value["type"]
        .as_str()
        .ok_or("pi-messages event has no type")?;
    let index = value["contentIndex"].as_u64();
    match kind {
        "start" => {}
        "text_start" => {
            content.insert(
                index.ok_or("text start has no contentIndex")?,
                StreamContent::Text(String::new()),
            );
        }
        "thinking_start" => {
            content.insert(
                index.ok_or("thinking start has no contentIndex")?,
                StreamContent::Thinking(String::new()),
            );
        }
        "text_delta" => {
            let delta = event_delta(&value)?;
            let slot = content
                .entry(index.ok_or("text delta has no contentIndex")?)
                .or_insert_with(|| StreamContent::Text(String::new()));
            let StreamContent::Text(text) = slot else {
                return Err("text delta does not match its content block".into());
            };
            text.push_str(delta);
            let _ = deltas.send(ModelDelta::Text(delta.to_owned()));
        }
        "thinking_delta" => {
            let delta = event_delta(&value)?;
            let slot = content
                .entry(index.ok_or("thinking delta has no contentIndex")?)
                .or_insert_with(|| StreamContent::Thinking(String::new()));
            let StreamContent::Thinking(text) = slot else {
                return Err("thinking delta does not match its content block".into());
            };
            text.push_str(delta);
            let _ = deltas.send(ModelDelta::Reasoning(delta.to_owned()));
        }
        "toolcall_start" => {
            content.insert(
                index.ok_or("tool call event has no contentIndex")?,
                StreamContent::Tool(String::new()),
            );
        }
        "toolcall_delta" => {
            let i = index.ok_or("tool call event has no contentIndex")?;
            let delta = event_delta(&value)?;
            let StreamContent::Tool(json) = content
                .entry(i)
                .or_insert_with(|| StreamContent::Tool(String::new()))
            else {
                return Err("tool delta does not match its content block".into());
            };
            json.push_str(delta);
            let _ = deltas.send(ModelDelta::ToolCall(delta.to_owned()));
        }
        "text_end" => {
            let i = index.ok_or("text end has no contentIndex")?;
            let mut text = rig::message::Text::from(
                value["content"].as_str().ok_or("text end has no content")?,
            );
            if let Some(signature) = value["contentSignature"].as_str() {
                text.additional_params = rig::message::AdditionalParams::from_entries([(
                    "textSignature",
                    Value::String(signature.to_string()),
                )]);
            }
            content.insert(
                i,
                StreamContent::Final(Box::new(AssistantContent::Text(text))),
            );
        }
        "thinking_end" => {
            let i = index.ok_or("thinking end has no contentIndex")?;
            if value["redacted"].as_bool() == Some(true) {
                let redacted = value["contentSignature"]
                    .as_str()
                    .or_else(|| value["content"].as_str())
                    .ok_or("redacted thinking end has no content")?;
                content.insert(
                    i,
                    StreamContent::Final(Box::new(AssistantContent::Reasoning(
                        Reasoning::redacted(redacted),
                    ))),
                );
                return Ok(None);
            }
            let text = value["content"]
                .as_str()
                .ok_or("thinking end has no content")?;
            let signature = value["contentSignature"].as_str().map(str::to_owned);
            content.insert(
                i,
                StreamContent::Final(Box::new(AssistantContent::Reasoning(
                    Reasoning::new_with_signature(text, signature),
                ))),
            );
        }
        "toolcall_end" => {
            let i = index.ok_or("tool call end has no contentIndex")?;
            let call = &value["toolCall"];
            let id = call["id"].as_str().ok_or("tool call has no id")?;
            let name = call["name"].as_str().ok_or("tool call has no name")?;
            let args = call["arguments"].clone();
            if !args.is_object() {
                return Err("tool call arguments must be a JSON object".into());
            }
            let mut tool_call = ToolCall::from_wire(id, ToolFunction::new(name.into(), args));
            tool_call.signature = call["thoughtSignature"].as_str().map(str::to_owned);
            let mut metadata = call.as_object().cloned().unwrap_or_default();
            for key in ["id", "name", "arguments", "thoughtSignature"] {
                metadata.remove(key);
            }
            tool_call.additional_params = (!metadata.is_empty()).then_some(Value::Object(metadata));
            content.insert(
                i,
                StreamContent::Final(Box::new(AssistantContent::ToolCall(tool_call))),
            );
        }
        "done" => {
            if content
                .values()
                .any(|item| matches!(item, StreamContent::Tool(_)))
            {
                return Err("pi-messages stream ended with an unfinished tool call".into());
            }
            let reason = finish_reason(value["reason"].as_str());
            let usage = usage(&value["usage"]);
            return Ok(Some(Ok(ModelResponse {
                content: content
                    .values_mut()
                    .filter_map(|item| match std::mem::take(item) {
                        StreamContent::Text(text) if !text.is_empty() => {
                            Some(AssistantContent::text(text))
                        }
                        StreamContent::Thinking(text) if !text.is_empty() => {
                            Some(AssistantContent::Reasoning(Reasoning::new(&text)))
                        }
                        StreamContent::Final(item) => Some(*item),
                        _ => None,
                    })
                    .collect(),
                usage: Some(usage),
                context_tokens: (usage.total_tokens > 0).then_some(usage.total_tokens as usize),
                finish_reason: Some(reason),
            })));
        }
        "error" => {
            return Ok(Some(Err(value["errorMessage"]
                .as_str()
                .unwrap_or("pi-messages backend error")
                .to_owned())));
        }
        _ => return Err(format!("unknown pi-messages event type {kind}")),
    }
    Ok(None)
}

fn event_delta(value: &Value) -> std::result::Result<&str, String> {
    value["delta"]
        .as_str()
        .ok_or_else(|| "delta event has no delta".into())
}
fn finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("length") => FinishReason::Length,
        Some("toolUse") => FinishReason::ToolCalls,
        Some("stop") => FinishReason::Stop,
        Some(other) => FinishReason::Other(other.into()),
        None => FinishReason::Other("missing".into()),
    }
}
fn usage(value: &Value) -> Usage {
    let n = |key| value[key].as_u64().unwrap_or(0);
    Usage {
        input_tokens: n("input"),
        output_tokens: n("output"),
        total_tokens: n("totalTokens"),
        cached_input_tokens: n("cacheRead"),
        cache_creation_input_tokens: n("cacheWrite"),
        tool_use_prompt_tokens: 0,
        reasoning_tokens: n("reasoning"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rig::message::{ImageMediaType, ToolCallId, ToolResult};

    #[tokio::test]
    async fn truncated_and_error_streams_never_return_partial_success() {
        let (tx, _) = mpsc::unbounded_channel();
        for body in [
            "data: {\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"partial\"}\n\ndata: [DONE]\n\n",
            "data: {\"type\":\"error\",\"reason\":\"error\",\"errorMessage\":\"failed\"}\n\n",
        ] {
            let response = http::Response::builder().status(200).body(body).unwrap();
            assert!(collect_stream(response.into(), &tx).await.is_err());
        }
    }

    #[tokio::test]
    async fn radius_401_refreshes_once_then_streams_with_new_key() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in [
                (401, r#"{"error":{"message":"expired"}}"#),
                (
                    200,
                    r#"{"access_token":"fresh-key","refresh_token":"rotated","expires_in":3600}"#,
                ),
                (
                    200,
                    "data: {\"type\":\"text_end\",\"contentIndex\":0,\"content\":\"ok\"}\n\ndata: {\"type\":\"done\",\"reason\":\"stop\",\"usage\":{\"input\":2,\"output\":1,\"totalTokens\":3}}\n\n",
                ),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .unwrap_or("0")
                            .parse::<usize>()
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        let root = tempfile::tempdir().unwrap();
        let manager = ConfigManager::load_for_test(root.path(), root.path())
            .await
            .unwrap();
        let mut settings = manager
            .set_oauth(
                "radius",
                crate::oauth::OauthToken {
                    kind: "oauth".into(),
                    access: "old-key".into(),
                    refresh: "old-refresh".into(),
                    expires: i64::MAX,
                    extra: BTreeMap::from([("gateway".into(), json!(gateway))]),
                },
            )
            .await
            .unwrap();
        settings.provider = "radius".into();
        settings.auth_kind = AuthKind::Oauth;
        settings.api_key_source = ValueSource::Global;
        let model = CatalogModel {
            id: "balanced".into(),
            name: "Balanced".into(),
            api: "pi-messages".into(),
            provider: "radius".into(),
            base_url: format!("{gateway}/v1"),
            headers: Default::default(),
            metadata: Default::default(),
        };
        let backend = PiMessagesBackend::new(model, settings, None, manager.clone()).unwrap();
        let (tx, _) = mpsc::unbounded_channel();
        let response = backend
            .complete(
                ModelRequest {
                    system: "system".into(),
                    history: vec![Message::user("hello")],
                    tools: vec![],
                    estimated_context: 1,
                    max_output_tokens: Some(10),
                },
                tx,
            )
            .await
            .unwrap();
        assert_eq!(response.context_tokens, Some(3));
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /v1/messages "));
        assert!(requests[0].contains("Bearer old-key"));
        assert!(requests[1].starts_with("POST /v1/oauth/token "));
        assert!(requests[2].contains("Bearer fresh-key"));
        assert_eq!(
            manager.oauth_token("radius").await.unwrap().refresh,
            "rotated"
        );
    }

    async fn stream_fixture(line_ending: &str) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = format!(
            "data: {{\"type\":\"text_start\",\"contentIndex\":0}}{0}{0}data: {{\"type\":\"text_delta\",\"contentIndex\":0,\"delta\":\"ok\"}}{0}{0}data: {{\"type\":\"done\",\"reason\":\"stop\",\"usage\":{{}}}}",
            line_ending
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            for byte in body.bytes() {
                socket
                    .write_all(format!("1\r\n{}\r\n", char::from(byte)).as_bytes())
                    .await
                    .unwrap();
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        });
        reqwest::get(format!("http://{address}")).await.unwrap()
    }

    #[tokio::test]
    async fn actual_http_sse_handles_lf_crlf_one_byte_chunks_and_eof_event() {
        for ending in ["\n", "\r\n"] {
            let (tx, _) = mpsc::unbounded_channel();
            let response = collect_stream(stream_fixture(ending).await, &tx)
                .await
                .unwrap();
            assert!(
                matches!(&response.content[0], AssistantContent::Text(text) if text.text == "ok")
            );
        }
    }
    #[test]
    fn body_preserves_native_history_and_options() {
        let mut model = CatalogModel {
            id: "auto".into(),
            name: "Auto".into(),
            api: "pi-messages".into(),
            provider: "custom".into(),
            base_url: "https://example.test/v1".into(),
            headers: Default::default(),
            metadata: Default::default(),
        };
        model.metadata.insert("reasoning".into(), true.into());
        let result = ToolResult {
            call: ToolCallId::new("call_1").unwrap(),
            provider: None,
            name: "read".into(),
            content: vec![
                ToolResultContent::text("ok"),
                ToolResultContent::image_base64("abc", Some(ImageMediaType::PNG), None),
            ],
        };
        let request = ModelRequest {
            system: "system".into(),
            history: vec![
                Message::user("hi"),
                Message::User {
                    content: vec![UserContent::ToolResult(result)],
                },
            ],
            tools: vec![],
            estimated_context: 1,
            max_output_tokens: Some(99),
        };
        let body = request_payload(&model, &request, ThinkingLevel::High, Some("session")).unwrap();
        assert_eq!(
            body["options"],
            json!({"maxTokens":99,"reasoning":"high","sessionId":"session"})
        );
        assert_eq!(body["context"]["messages"][1]["role"], "toolResult");
        assert_eq!(
            body["context"]["messages"][1]["content"][1]["mimeType"],
            "image/png"
        );
    }
    #[tokio::test]
    async fn chunked_events_use_authoritative_end_and_require_terminal() {
        let (tx, _) = mpsc::unbounded_channel();
        let mut content = BTreeMap::new();
        apply_event(
            json!({"type":"text_delta","contentIndex":0,"delta":"wrong"}),
            &mut content,
            &tx,
        )
        .unwrap();
        apply_event(
            json!({"type":"text_end","contentIndex":0,"content":"right"}),
            &mut content,
            &tx,
        )
        .unwrap();
        let done = apply_event(
            json!({"type":"done","reason":"stop","usage":{"input":2,"output":1,"totalTokens":3}}),
            &mut content,
            &tx,
        )
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(matches!(&done.content[0],AssistantContent::Text(t) if t.text=="right"));
        assert!(parse_event(b"data: [DONE]").unwrap().is_none());
    }
    #[test]
    fn tool_and_thinking_end_override_streamed_parts() {
        let (tx, _) = mpsc::unbounded_channel();
        let mut content = BTreeMap::new();
        apply_event(json!({"type":"thinking_end","contentIndex":0,"content":"thought","contentSignature":"sig"}),&mut content,&tx).unwrap();
        apply_event(
            json!({"type":"toolcall_start","contentIndex":1,"id":"c","toolName":"read"}),
            &mut content,
            &tx,
        )
        .unwrap();
        apply_event(
            json!({"type":"toolcall_delta","contentIndex":1,"delta":"{bad"}),
            &mut content,
            &tx,
        )
        .unwrap();
        apply_event(json!({"type":"toolcall_end","contentIndex":1,"toolCall":{"id":"c","name":"read","arguments":{"path":"x"}}}),&mut content,&tx).unwrap();
        let Some(StreamContent::Final(call)) = content.get(&1) else {
            panic!("missing final tool call");
        };
        assert!(
            matches!(call.as_ref(), AssistantContent::ToolCall(c) if c.function.arguments["path"] == "x")
        );
    }
}
