use crate::catalog::{CatalogModel, ThinkingLevel};
use crate::config::ConfigManager;
use crate::oauth::OauthToken;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use rig::http_client::sse::BoxedStream;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

const PROVIDER: &str = "antigravity";
const PROD_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
const DAILY_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
const SANDBOX_BASE_URL: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
const STREAM_PATH: &str = "/v1internal:streamGenerateContent?alt=sse";
const DUMMY_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";
const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_ANTIGRAVITY_VERSION: &str = "4.3.0";
const ANTHROPIC_BETA: &str =
    "claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AntigravityRoute {
    pub(super) model: String,
    pub(super) thinking_budget: u64,
    pub(super) max_output_tokens: u64,
    pub(super) include_thoughts: bool,
}

pub(super) fn resolve_route(
    model: &CatalogModel,
    thinking: ThinkingLevel,
) -> Option<AntigravityRoute> {
    let mapped_level = model
        .thinking_level(thinking)
        .and_then(Value::as_str)
        .unwrap_or_else(|| thinking.as_str());
    let route = model
        .compat("antigravityRoutes")
        .and_then(|routes| routes.get(mapped_level))
        .and_then(Value::as_object)?;
    let upstream = route
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())?;
    let thinking_budget = route
        .get("thinkingBudget")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| thinking.budget());
    Some(AntigravityRoute {
        model: upstream.to_string(),
        thinking_budget,
        max_output_tokens: route
            .get("maxOutputTokens")
            .and_then(Value::as_u64)
            .filter(|limit| *limit > 0)
            .unwrap_or_else(|| model.max_tokens()),
        include_thoughts: route
            .get("includeThoughts")
            .and_then(Value::as_bool)
            .unwrap_or(thinking_budget > 0),
    })
}

#[async_trait]
trait CredentialStore: Send + Sync {
    async fn token(&self) -> Result<OauthToken>;
    async fn refresh(&self) -> Result<OauthToken>;
}

struct ConfigCredentials {
    manager: Arc<ConfigManager>,
}

#[async_trait]
impl CredentialStore for ConfigCredentials {
    async fn token(&self) -> Result<OauthToken> {
        self.manager.oauth_token(PROVIDER).await
    }

    async fn refresh(&self) -> Result<OauthToken> {
        self.manager.force_refresh_oauth(PROVIDER).await
    }
}

#[derive(Clone)]
pub(super) struct AntigravityTransport {
    credentials: Arc<dyn CredentialStore>,
    route: AntigravityRoute,
    session_id: String,
    identity_prompt: Option<String>,
    endpoints: Endpoints,
}

impl std::fmt::Debug for AntigravityTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AntigravityTransport")
            .field("route", &self.route)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl AntigravityTransport {
    pub(super) fn new(
        model: &CatalogModel,
        thinking: ThinkingLevel,
        session_id: Option<&str>,
        manager: Arc<ConfigManager>,
    ) -> Result<Self> {
        let route = resolve_route(model, thinking).ok_or_else(|| {
            anyhow!(
                "Antigravity model {}/{} has no route for effort {}",
                model.provider,
                model.id,
                thinking
            )
        })?;
        let identity_prompt = env::var("ANTIGRAVITY_IDENTITY_PROMPT")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let affinity = session_id
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        Ok(Self {
            credentials: Arc::new(ConfigCredentials { manager }),
            route,
            session_id: stable_session_id(&affinity),
            identity_prompt,
            endpoints: Endpoints::default(),
        })
    }

    pub(super) async fn send(
        &self,
        client: reqwest::Client,
        request: Request<Bytes>,
    ) -> rig::http_client::Result<rig::http_client::StreamingResponse> {
        let inner = serde_json::from_slice::<Value>(request.body())
            .context("Rig produced an invalid Gemini request")
            .and_then(|value| {
                value
                    .is_object()
                    .then_some(value)
                    .ok_or_else(|| anyhow!("Rig produced a non-object Gemini request"))
            })
            .map_err(instance_error)?;
        let mut token = self.credentials.token().await.map_err(instance_error)?;
        let mut refreshed = false;
        if token.expired() {
            token = self.credentials.refresh().await.map_err(instance_error)?;
            refreshed = true;
        }
        let mut signature_repaired = false;
        let mut without_project_header = false;
        let mut inner = inner;

        loop {
            let body = self
                .envelope(inner.clone(), &token)
                .map_err(instance_error)?;
            let endpoints = self.endpoints.inference_urls();
            let mut restart = false;
            for (index, endpoint) in endpoints.iter().enumerate() {
                let response = match send_once(
                    &client,
                    &request,
                    &token,
                    endpoint,
                    body.clone(),
                    &self.route,
                    !without_project_header,
                )
                .await
                {
                    Ok(response) => response,
                    Err(_) if index + 1 < endpoints.len() => continue,
                    Err(error) => return Err(instance_error(error)),
                };
                if response.status().is_success() {
                    return streaming_response(response, self.route.model.starts_with("claude-"));
                }

                let status = response.status();
                let headers = response.headers().clone();
                let error_body = response.text().await.unwrap_or_default();
                if status == StatusCode::UNAUTHORIZED && !refreshed {
                    token = self.credentials.refresh().await.map_err(instance_error)?;
                    refreshed = true;
                    restart = true;
                    break;
                }
                if status == StatusCode::FORBIDDEN && !without_project_header {
                    without_project_header = true;
                    restart = true;
                    break;
                }
                if status == StatusCode::BAD_REQUEST
                    && !signature_repaired
                    && self.route.model.starts_with("gemini-")
                    && signature_error(&error_body)
                    && repair_thought_signatures(&mut inner)
                {
                    signature_repaired = true;
                    restart = true;
                    break;
                }
                if should_try_next_endpoint(status) && index + 1 < endpoints.len() {
                    continue;
                }
                return Err(rig::http_client::Error::InvalidStatusCodeWithDetails {
                    status,
                    body: error_body,
                    headers: Box::new(headers),
                });
            }
            if restart {
                continue;
            }
            return Err(instance_error("all Antigravity endpoints failed"));
        }
    }

    fn envelope(&self, mut request: Value, token: &OauthToken) -> Result<Bytes> {
        prepare_inner_request(
            &mut request,
            &self.session_id,
            &self.route.model,
            self.identity_prompt.as_deref(),
        )?;
        let project = extra_string(token, "projectId")
            .ok_or_else(|| anyhow!("Antigravity OAuth credential has no projectId"))?;
        Ok(Bytes::from(serde_json::to_vec(&json!({
            "project": project,
            "requestId": format!(
                "agent/{}/{}",
                chrono::Utc::now().timestamp_millis(),
                &uuid::Uuid::now_v7().simple().to_string()[..8]
            ),
            "userAgent": "antigravity",
            "requestType": "agent",
            "model": self.route.model,
            "request": request,
            "enabledCreditTypes": ["GOOGLE_ONE_AI"]
        }))?))
    }
}

#[derive(Clone, Debug)]
struct Endpoints {
    sandbox: String,
    prod: String,
    daily: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            sandbox: SANDBOX_BASE_URL.to_string(),
            prod: PROD_BASE_URL.to_string(),
            daily: DAILY_BASE_URL.to_string(),
        }
    }
}

impl Endpoints {
    fn inference_urls(&self) -> [String; 3] {
        [&self.sandbox, &self.daily, &self.prod]
            .map(|base| format!("{}{STREAM_PATH}", base.trim_end_matches('/')))
    }
}

fn should_try_next_endpoint(status: StatusCode) -> bool {
    matches!(status, StatusCode::REQUEST_TIMEOUT | StatusCode::NOT_FOUND)
        || status.is_server_error()
}

async fn send_once(
    client: &reqwest::Client,
    original: &Request<Bytes>,
    token: &OauthToken,
    endpoint: &str,
    body: Bytes,
    route: &AntigravityRoute,
    include_project_header: bool,
) -> Result<reqwest::Response> {
    let user_agent = env::var("ANTIGRAVITY_USER_AGENT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(default_generation_user_agent);
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", token.access))?,
    );
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_str(&user_agent)?,
    );
    headers.insert("x-client-name", HeaderValue::from_static("antigravity"));
    headers.insert(
        "x-client-version",
        HeaderValue::from_str(&antigravity_version())?,
    );
    headers.insert(
        "x-vscode-sessionid",
        HeaderValue::from_str(process_session_id())?,
    );
    if let Some(machine_id) = machine_id() {
        headers.insert("x-machine-id", HeaderValue::from_str(&machine_id)?);
    }
    if include_project_header && let Some(project) = extra_string(token, "projectId") {
        headers.insert("x-goog-user-project", HeaderValue::from_str(&project)?);
    }
    if route.model.starts_with("claude-") {
        headers.insert("anthropic-beta", HeaderValue::from_static(ANTHROPIC_BETA));
    }
    let body =
        reqwest::Body::wrap_stream(stream::once(
            async move { Ok::<Bytes, std::io::Error>(body) },
        ));
    let response = client
        .request(original.method().clone(), endpoint)
        .headers(headers)
        .body(body)
        .send()
        .await?;
    Ok(response)
}

fn antigravity_version() -> String {
    env::var("ANTIGRAVITY_USER_AGENT_VERSION")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_ANTIGRAVITY_VERSION.to_string())
}

fn default_generation_user_agent() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "Macintosh; Intel Mac OS X 10_15_7",
        "windows" => "Windows NT 10.0; Win64; x64",
        _ => "X11; Linux x86_64",
    };
    format!(
        "Antigravity/{} ({platform}) Chrome/132.0.6834.160 Electron/39.2.3",
        antigravity_version()
    )
}

fn process_session_id() -> &'static str {
    static SESSION_ID: OnceLock<String> = OnceLock::new();
    SESSION_ID
        .get_or_init(|| uuid::Uuid::now_v7().to_string())
        .as_str()
}

fn machine_id() -> Option<String> {
    if let Ok(value) = env::var("ANTIGRAVITY_MACHINE_ID") {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(value) = std::fs::read_to_string("/etc/machine-id") {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn streaming_response(
    response: reqwest::Response,
    inject_claude_ids: bool,
) -> rig::http_client::Result<rig::http_client::StreamingResponse> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let source: SourceStream = Box::pin(response.bytes_stream());
    let body = unwrap_sse_stream(source, inject_claude_ids);
    let mut output = Response::builder().status(status).version(version);
    if let Some(output_headers) = output.headers_mut() {
        *output_headers = headers;
    }
    output.body(body).map_err(rig::http_client::Error::Protocol)
}

type SourceStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

struct SseState {
    source: SourceStream,
    buffer: Vec<u8>,
    eof: bool,
    inject_claude_ids: bool,
}

fn unwrap_sse_stream(source: SourceStream, inject_claude_ids: bool) -> BoxedStream {
    let state = SseState {
        source,
        buffer: Vec::new(),
        eof: false,
        inject_claude_ids,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(line) = take_line(&mut state.buffer, state.eof) {
                let transformed = transform_sse_line(&line, state.inject_claude_ids)
                    .map(Bytes::from)
                    .map_err(instance_error);
                return Some((transformed, state));
            }
            if state.eof {
                return None;
            }
            match state.source.next().await {
                Some(Ok(chunk)) => {
                    state.buffer.extend_from_slice(&chunk);
                    if state.buffer.len() > MAX_SSE_LINE_BYTES && !state.buffer.contains(&b'\n') {
                        state.eof = true;
                        let error = instance_error(anyhow!(
                            "Antigravity SSE line exceeds {MAX_SSE_LINE_BYTES} bytes"
                        ));
                        return Some((Err(error), state));
                    }
                }
                Some(Err(error)) => {
                    state.eof = true;
                    return Some((Err(instance_error(error)), state));
                }
                None => state.eof = true,
            }
        }
    }))
}

fn take_line(buffer: &mut Vec<u8>, eof: bool) -> Option<Vec<u8>> {
    if let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
        return Some(buffer.drain(..=end).collect());
    }
    if eof && !buffer.is_empty() {
        return Some(std::mem::take(buffer));
    }
    None
}

fn transform_sse_line(line: &[u8], inject_claude_ids: bool) -> Result<Vec<u8>> {
    let (content, ending) = if let Some(content) = line.strip_suffix(b"\r\n") {
        (content, b"\r\n".as_slice())
    } else if let Some(content) = line.strip_suffix(b"\n") {
        (content, b"\n".as_slice())
    } else {
        (line, b"".as_slice())
    };
    let Some(payload) = content.strip_prefix(b"data:") else {
        return Ok(line.to_vec());
    };
    let payload = payload.strip_prefix(b" ").unwrap_or(payload).trim_ascii();
    if payload.is_empty() || payload == b"[DONE]" {
        return Ok(line.to_vec());
    }
    let Ok(mut envelope) = serde_json::from_slice::<Value>(payload) else {
        return Ok(line.to_vec());
    };
    let Some(object) = envelope.as_object_mut() else {
        return Ok(line.to_vec());
    };
    let Some(mut response) = object.remove("response") else {
        return Ok(line.to_vec());
    };
    if let Some(response_object) = response.as_object_mut() {
        for key in ["responseId", "modelVersion"] {
            if !response_object.contains_key(key)
                && let Some(value) = object.get(key)
            {
                response_object.insert(key.to_string(), value.clone());
            }
        }
        if inject_claude_ids {
            inject_missing_claude_tool_ids(response_object);
        }
    }
    let mut transformed = b"data: ".to_vec();
    transformed.extend_from_slice(&serde_json::to_vec(&response)?);
    transformed.extend_from_slice(ending);
    Ok(transformed)
}

fn inject_missing_claude_tool_ids(response: &mut Map<String, Value>) {
    let Some(candidates) = response.get_mut("candidates").and_then(Value::as_array_mut) else {
        return;
    };
    for candidate in candidates {
        let Some(parts) = candidate
            .get_mut("content")
            .and_then(|content| content.get_mut("parts"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let mut ids = std::collections::HashMap::<String, usize>::new();
        for part in parts {
            let Some(call) = part.get_mut("functionCall").and_then(Value::as_object_mut) else {
                continue;
            };
            if call
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
            {
                continue;
            }
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let count = ids.entry(name.to_string()).or_default();
            call.insert("id".to_string(), json!(format!("call_{name}_{count}")));
            *count += 1;
        }
    }
}

fn prepare_inner_request(
    request: &mut Value,
    session_id: &str,
    upstream_model: &str,
    identity_prompt: Option<&str>,
) -> Result<()> {
    let body = request
        .as_object_mut()
        .ok_or_else(|| anyhow!("Antigravity inner request is not an object"))?;
    body.insert(
        "sessionId".to_string(),
        Value::String(session_id.to_string()),
    );
    filter_empty_contents(body, "contents");
    if let Some(system) = body.get_mut("systemInstruction") {
        filter_empty_content(system);
    }
    clean_tool_schemas(body);
    normalize_request_parts(body, upstream_model);
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if has_tools {
        body.insert(
            "toolConfig".to_string(),
            json!({
                "functionCallingConfig": {"mode": "VALIDATED"},
                "includeServerSideToolInvocations": true
            }),
        );
        body.insert(
            "tool_config".to_string(),
            json!({
                "function_calling_config": {"mode": "VALIDATED"},
                "include_server_side_tool_invocations": true
            }),
        );
    }
    if let Some(prompt) = identity_prompt {
        let system = body
            .entry("systemInstruction")
            .or_insert_with(|| json!({"role": "user", "parts": []}));
        let Some(system) = system.as_object_mut() else {
            bail!("Antigravity systemInstruction is not an object")
        };
        let parts = system
            .entry("parts")
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(parts) = parts.as_array_mut() else {
            bail!("Antigravity systemInstruction.parts is not an array")
        };
        parts.insert(0, json!({"text": prompt}));
    }
    Ok(())
}

fn filter_empty_contents(body: &mut Map<String, Value>, key: &str) {
    let Some(contents) = body.get_mut(key).and_then(Value::as_array_mut) else {
        return;
    };
    for content in contents.iter_mut() {
        filter_empty_content(content);
    }
    contents.retain(|content| {
        content
            .get("parts")
            .and_then(Value::as_array)
            .is_some_and(|parts| !parts.is_empty())
    });
}

fn filter_empty_content(content: &mut Value) {
    let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut) else {
        return;
    };
    parts.retain(|part| {
        let Some(part) = part.as_object() else {
            return false;
        };
        part.iter().any(|(key, value)| {
            key != "text" || value.as_str().is_some_and(|text| !text.trim().is_empty())
        })
    });
}

fn clean_tool_schemas(body: &mut Map<String, Value>) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        for key in ["functionDeclarations", "function_declarations"] {
            let Some(declarations) = tool.get_mut(key).and_then(Value::as_array_mut) else {
                continue;
            };
            for declaration in declarations.iter_mut().filter_map(Value::as_object_mut) {
                if let Some(parameters) = declaration
                    .remove("parametersJsonSchema")
                    .or_else(|| declaration.remove("parameters_json_schema"))
                {
                    declaration.insert("parameters".to_string(), parameters);
                }
                if let Some(parameters) = declaration.get_mut("parameters") {
                    clean_schema(parameters);
                }
            }
        }
    }
}

fn clean_schema(schema: &mut Value) {
    if !schema.is_object() {
        *schema = json!({"type": "object", "properties": {}});
        return;
    }
    let root_is_untyped = schema.as_object().is_some_and(|object| {
        ![
            "type",
            "properties",
            "items",
            "enum",
            "$ref",
            "allOf",
            "anyOf",
            "oneOf",
        ]
        .into_iter()
        .any(|key| object.contains_key(key))
    });
    let mut definitions = Map::new();
    collect_schema_definitions(schema, &mut definitions);
    clean_schema_node(schema, &definitions, 0);
    if root_is_untyped && let Some(object) = schema.as_object_mut() {
        object.insert("type".to_string(), Value::String("object".to_string()));
        object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

fn collect_schema_definitions(value: &Value, definitions: &mut Map<String, Value>) {
    match value {
        Value::Object(object) => {
            for key in ["$defs", "definitions"] {
                if let Some(Value::Object(found)) = object.get(key) {
                    for (name, schema) in found {
                        definitions
                            .entry(name.clone())
                            .or_insert_with(|| schema.clone());
                    }
                }
            }
            for (key, value) in object {
                if key != "$defs" && key != "definitions" {
                    collect_schema_definitions(value, definitions);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_schema_definitions(value, definitions);
            }
        }
        _ => {}
    }
}

fn clean_schema_node(schema: &mut Value, definitions: &Map<String, Value>, depth: usize) {
    if depth > 32 {
        return;
    }
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if let Some(reference) = object
        .remove("$ref")
        .and_then(|value| value.as_str().map(str::to_string))
    {
        if let Some(name) = reference.rsplit('/').next()
            && let Some(mut resolved) = definitions.get(name).cloned()
        {
            clean_schema_node(&mut resolved, definitions, depth + 1);
            if let Some(resolved) = resolved.as_object_mut() {
                resolved.append(object);
                *object = std::mem::take(resolved);
            }
        } else {
            object.insert("type".to_string(), Value::String("string".to_string()));
            append_schema_description(object, &format!("unresolved reference: {reference}"));
        }
    }
    for union in ["allOf", "anyOf", "oneOf"] {
        let Some(branches) = object
            .remove(union)
            .and_then(|value| value.as_array().cloned())
        else {
            continue;
        };
        let mut cleaned = Vec::new();
        for mut branch in branches {
            if branch.get("type").and_then(Value::as_str) == Some("null") {
                continue;
            }
            clean_schema_node(&mut branch, definitions, depth + 1);
            let Some(branch_object) = branch.as_object() else {
                continue;
            };
            if branch_object.get("type").and_then(Value::as_str) == Some("null") {
                continue;
            }
            cleaned.push(branch);
        }
        if union == "allOf" {
            for branch in &mut cleaned {
                if let Some(branch) = branch.as_object_mut() {
                    merge_schema_object(object, branch);
                }
            }
        } else if let Some(mut branch) = cleaned
            .into_iter()
            .max_by_key(schema_complexity)
            .and_then(|branch| branch.as_object().cloned())
        {
            merge_schema_object(object, &mut branch);
        }
    }
    let mut nullable_properties = std::collections::HashSet::new();
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        properties.retain(|_, property| property.is_object());
        for (name, property) in properties.iter_mut() {
            if schema_allows_null(property) {
                nullable_properties.insert(name.clone());
            }
            clean_schema_node(property, definitions, depth + 1);
        }
    }
    let object_like = object.contains_key("properties")
        || object.get("type").and_then(Value::as_str) == Some("object");
    if object_like {
        object.remove("items");
    }
    if let Some(items) = object.get_mut("items") {
        if let Some(tuple) = items.as_array_mut() {
            *items = tuple
                .iter()
                .find(|item| {
                    item.is_object() && item.get("type").and_then(Value::as_str) != Some("null")
                })
                .cloned()
                .unwrap_or_else(|| json!({"type": "string"}));
        }
        if items.is_object() {
            clean_schema_node(items, definitions, depth + 1);
        } else {
            object.remove("items");
        }
    }
    let hints = [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "minItems",
        "maxItems",
        "multipleOf",
        "uniqueItems",
    ]
    .into_iter()
    .filter_map(|key| object.get(key).map(|value| format!("{key}: {value}")))
    .collect::<Vec<_>>();
    if !hints.is_empty() {
        let description = object
            .entry("description")
            .or_insert_with(|| Value::String(String::new()));
        if let Some(previous) = description.as_str().map(str::to_string) {
            *description = Value::String(format!(
                "{}{}[Constraint: {}]",
                previous,
                if previous.is_empty() { "" } else { " " },
                hints.join(", ")
            ));
        }
    }
    const ALLOWED: [&str; 7] = [
        "type",
        "description",
        "properties",
        "required",
        "items",
        "enum",
        "title",
    ];
    object.retain(|key, _| ALLOWED.contains(&key.as_str()));
    let fallback_type = if object.contains_key("properties") {
        "object"
    } else if object.contains_key("items") {
        "array"
    } else {
        "string"
    };
    const VALID_TYPES: [&str; 6] = ["string", "number", "integer", "boolean", "array", "object"];
    if let Some(kind) = object.get_mut("type") {
        let mut nullable = false;
        let selected = match kind {
            Value::String(kind) => {
                let kind = kind.trim().to_ascii_lowercase();
                nullable = kind == "null";
                VALID_TYPES.contains(&kind.as_str()).then_some(kind)
            }
            Value::Array(kinds) => kinds.iter().find_map(|kind| {
                let kind = kind.as_str()?.trim().to_ascii_lowercase();
                if kind == "null" {
                    nullable = true;
                    None
                } else if VALID_TYPES.contains(&kind.as_str()) {
                    Some(kind)
                } else {
                    None
                }
            }),
            _ => None,
        };
        *kind = Value::String(selected.unwrap_or_else(|| fallback_type.to_string()));
        if nullable {
            append_schema_description(object, "nullable");
        }
    } else {
        object.insert("type".to_string(), Value::String(fallback_type.to_string()));
    }
    if object.get("type").and_then(Value::as_str) == Some("object") {
        object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if let Some(values) = object.get_mut("enum").and_then(Value::as_array_mut) {
        for value in values {
            if !value.is_string() {
                *value = Value::String(if value.is_null() {
                    "null".to_string()
                } else {
                    value.to_string()
                });
            }
        }
    }
    let property_names = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect::<Vec<_>>());
    if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut)
        && let Some(property_names) = property_names
    {
        required.retain(|name| {
            name.as_str().is_some_and(|name| {
                property_names.iter().any(|property| property == name)
                    && !nullable_properties.contains(name)
            })
        });
    }
}

fn schema_complexity(schema: &Value) -> usize {
    let Some(object) = schema.as_object() else {
        return 0;
    };
    object
        .get("properties")
        .and_then(Value::as_object)
        .map_or(0, |properties| properties.len() * 10)
        + object.len()
}

fn schema_allows_null(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    let nullable_type = match object.get("type") {
        Some(Value::String(kind)) => kind.eq_ignore_ascii_case("null"),
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| {
            kind.as_str()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("null"))
        }),
        _ => false,
    };
    nullable_type
        || ["anyOf", "oneOf"]
            .into_iter()
            .filter_map(|key| object.get(key).and_then(Value::as_array))
            .flatten()
            .any(schema_allows_null)
}

fn append_schema_description(object: &mut Map<String, Value>, hint: &str) {
    let description = object
        .entry("description")
        .or_insert_with(|| Value::String(String::new()));
    if let Some(previous) = description.as_str()
        && !previous.contains(hint)
    {
        *description = Value::String(format!(
            "{}{}({hint})",
            previous,
            if previous.is_empty() { "" } else { " " }
        ));
    }
}

fn merge_schema_object(target: &mut Map<String, Value>, source: &mut Map<String, Value>) {
    for (key, value) in std::mem::take(source) {
        match (key.as_str(), target.get_mut(&key)) {
            ("properties", Some(Value::Object(existing))) => {
                if let Value::Object(properties) = value {
                    existing.extend(properties);
                }
            }
            ("required", Some(Value::Array(existing))) => {
                if let Value::Array(required) = value {
                    for name in required {
                        if !existing.contains(&name) {
                            existing.push(name);
                        }
                    }
                }
            }
            (_, None) => {
                target.insert(key, value);
            }
            _ => {}
        }
    }
}

fn normalize_request_parts(body: &mut Map<String, Value>, upstream_model: &str) {
    let flash = upstream_model.starts_with("gemini-") && upstream_model.contains("flash");
    let claude = upstream_model.starts_with("claude-");
    let Some(contents) = body.get_mut("contents").and_then(Value::as_array_mut) else {
        return;
    };
    for content in contents {
        let mut claude_ids = std::collections::HashMap::<String, usize>::new();
        let Some(parts) = content.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts {
            let Some(object) = part.as_object_mut() else {
                continue;
            };
            mirror_thought_signature(object);
            if flash
                && object.contains_key("functionCall")
                && !object.contains_key("thoughtSignature")
            {
                object.insert(
                    "thoughtSignature".to_string(),
                    Value::String(DUMMY_THOUGHT_SIGNATURE.to_string()),
                );
                object.insert(
                    "thought_signature".to_string(),
                    Value::String(DUMMY_THOUGHT_SIGNATURE.to_string()),
                );
            }
            if claude {
                for key in ["functionCall", "functionResponse"] {
                    let Some(call) = object.get_mut(key).and_then(Value::as_object_mut) else {
                        continue;
                    };
                    if call.get("id").and_then(Value::as_str).is_some() {
                        continue;
                    }
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let count = claude_ids.entry(name.to_string()).or_default();
                    call.insert("id".to_string(), json!(format!("call_{name}_{count}")));
                    *count += 1;
                }
            }
        }
    }
}

fn mirror_thought_signature(object: &mut Map<String, Value>) {
    let signature = object
        .get("thoughtSignature")
        .or_else(|| object.get("thought_signature"))
        .cloned();
    if let Some(signature) = signature {
        object
            .entry("thoughtSignature")
            .or_insert_with(|| signature.clone());
        object.entry("thought_signature").or_insert(signature);
    }
}

fn repair_thought_signatures(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Object(object) => {
            if object.contains_key("functionCall")
                || object.contains_key("thoughtSignature")
                || object.contains_key("thought_signature")
            {
                object.insert(
                    "thoughtSignature".to_string(),
                    Value::String(DUMMY_THOUGHT_SIGNATURE.to_string()),
                );
                object.insert(
                    "thought_signature".to_string(),
                    Value::String(DUMMY_THOUGHT_SIGNATURE.to_string()),
                );
                changed = true;
            }
            for value in object.values_mut() {
                changed |= repair_thought_signatures(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                changed |= repair_thought_signatures(value);
            }
        }
        _ => {}
    }
    changed
}

fn signature_error(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    body.contains("signature")
        || (body.contains("expected")
            && (body.contains("thinking") || body.contains("redacted_thinking")))
}

fn stable_session_id(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let number =
        u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 prefix")) & i64::MAX as u64;
    format!("-{number}")
}

fn extra_string(token: &OauthToken, key: &str) -> Option<String> {
    token
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn instance_error(error: impl std::fmt::Display) -> rig::http_client::Error {
    rig::http_client::Error::Instance(Box::new(std::io::Error::other(error.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct TestCredentials {
        current: OauthToken,
        refreshed: OauthToken,
        refreshes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CredentialStore for TestCredentials {
        async fn token(&self) -> Result<OauthToken> {
            Ok(self.current.clone())
        }

        async fn refresh(&self) -> Result<OauthToken> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(self.refreshed.clone())
        }
    }

    struct CapturedRequest {
        head: String,
        body: Value,
    }

    fn decode_chunked_body(bytes: &[u8]) -> Option<Vec<u8>> {
        let mut decoded = Vec::new();
        let mut offset = 0;
        loop {
            let line_end = bytes[offset..]
                .windows(2)
                .position(|part| part == b"\r\n")?
                + offset;
            let size = std::str::from_utf8(&bytes[offset..line_end])
                .ok()?
                .split(';')
                .next()
                .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())?;
            offset = line_end + 2;
            if size == 0 {
                return (bytes.get(offset..offset + 2) == Some(b"\r\n")).then_some(decoded);
            }
            let data_end = offset.checked_add(size)?;
            if bytes.get(data_end..data_end + 2) != Some(b"\r\n") {
                return None;
            }
            decoded.extend_from_slice(bytes.get(offset..data_end)?);
            offset = data_end + 2;
        }
    }

    async fn mock_server(
        responses: Vec<(u16, &'static str, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<CapturedRequest>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut captured = Vec::new();
            for (status, content_type, response_body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (header_end, body) = loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let header_end = header_end + 4;
                        let head = String::from_utf8_lossy(&bytes[..header_end]);
                        let content_length = head.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        });
                        if let Some(content_length) = content_length
                            && bytes.len() >= header_end + content_length
                        {
                            break (
                                header_end,
                                bytes[header_end..header_end + content_length].to_vec(),
                            );
                        }
                        if head.lines().any(|line| {
                            line.split_once(':').is_some_and(|(name, value)| {
                                name.eq_ignore_ascii_case("transfer-encoding")
                                    && value.trim().eq_ignore_ascii_case("chunked")
                            })
                        }) && let Some(body) = decode_chunked_body(&bytes[header_end..])
                        {
                            break (header_end, body);
                        }
                    }
                };
                captured.push(CapturedRequest {
                    head: String::from_utf8_lossy(&bytes[..header_end]).into_owned(),
                    body: serde_json::from_slice(&body).unwrap(),
                });
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    503 => "Service Unavailable",
                    _ => "Response",
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                            response_body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            captured
        });
        (format!("http://{address}"), handle)
    }

    fn token(access: &str) -> OauthToken {
        OauthToken {
            kind: "oauth".to_string(),
            refresh: "refresh".to_string(),
            access: access.to_string(),
            expires: i64::MAX,
            extra: BTreeMap::from([
                (
                    "projectId".to_string(),
                    Value::String("project-1".to_string()),
                ),
                ("tier".to_string(), Value::String("free-tier".to_string())),
            ]),
        }
    }

    fn test_transport(
        endpoint: String,
        refreshes: Arc<AtomicUsize>,
        upstream_model: &str,
    ) -> AntigravityTransport {
        AntigravityTransport {
            credentials: Arc::new(TestCredentials {
                current: token("old-token"),
                refreshed: token("new-token"),
                refreshes,
            }),
            route: AntigravityRoute {
                model: upstream_model.to_string(),
                thinking_budget: 10_000,
                max_output_tokens: 65_536,
                include_thoughts: true,
            },
            session_id: "-123".to_string(),
            identity_prompt: None,
            endpoints: Endpoints {
                sandbox: endpoint.clone(),
                prod: endpoint.clone(),
                daily: endpoint,
            },
        }
    }

    fn test_request(body: Value) -> Request<Bytes> {
        Request::post("https://ignored.invalid/v1beta/models/model:streamGenerateContent")
            .body(Bytes::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    async fn response_text(response: rig::http_client::StreamingResponse) -> String {
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn inner_request_gets_session_tools_schema_and_opt_in_identity() {
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hello"}]},
                {"role": "model", "parts": [{"text": ""}]}
            ],
            "tools": [{"functionDeclarations": [{
                "name": "read",
                "parametersJsonSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "$defs": {"Target": {"type": "integer", "minimum": 1}},
                    "properties": {
                        "uri": {"type": "string", "minLength": 1},
                        "body": {"type": "string"},
                        "target": {"$ref": "#/$defs/Target"},
                        "optional": {"type": ["string", "null"]},
                        "choice": {"anyOf": [
                            {"type": "string"},
                            {"type": "object", "properties": {"detail": {"type": "string"}}}
                        ]},
                        "invalid": false
                    },
                    "required": ["uri", "body", "target", "optional", "invalid", "missing"]
                }
            }, {
                "name": "exec",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "body": {"type": "string"}
                    },
                    "required": ["body"]
                }
            }]}],
            "toolConfig": null
        });
        prepare_inner_request(
            &mut request,
            "-123",
            "gemini-3.7-flash-high",
            Some("explicit identity"),
        )
        .unwrap();
        assert_eq!(request["sessionId"], "-123");
        assert_eq!(request["contents"].as_array().unwrap().len(), 1);
        assert_eq!(
            request["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
        assert_eq!(
            request["toolConfig"]["includeServerSideToolInvocations"],
            true
        );
        assert_eq!(
            request["tool_config"]["function_calling_config"]["mode"],
            "VALIDATED"
        );
        let parameters = &request["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(parameters.get("additionalProperties").is_none());
        assert_eq!(parameters["required"], json!(["uri", "body", "target"]));
        assert!(parameters["properties"].get("invalid").is_none());
        assert_eq!(parameters["properties"]["body"]["type"], "string");
        assert_eq!(parameters["properties"]["target"]["type"], "integer");
        assert_eq!(parameters["properties"]["choice"]["type"], "object");
        assert!(
            parameters["properties"]["uri"]["description"]
                .as_str()
                .unwrap()
                .contains("minLength")
        );
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            "explicit identity"
        );
        assert_eq!(
            request["tools"][0]["functionDeclarations"][1]["parameters"]["properties"]["body"]["type"],
            "string"
        );
        assert_eq!(
            request["tools"][0]["functionDeclarations"][1]["parameters"]["required"],
            json!(["body"])
        );
    }

    #[test]
    fn wrapped_sse_is_unwrapped_across_line_endings() {
        let line = b"data: {\"response\":{\"candidates\":[]},\"responseId\":\"r1\"}\r\n";
        assert_eq!(
            String::from_utf8(transform_sse_line(line, false).unwrap()).unwrap(),
            "data: {\"candidates\":[],\"responseId\":\"r1\"}\r\n"
        );
        assert_eq!(transform_sse_line(b": ping\n", false).unwrap(), b": ping\n");
        assert_eq!(
            transform_sse_line(b"data: not-json\n", false).unwrap(),
            b"data: not-json\n"
        );
    }

    #[test]
    fn claude_sse_gets_stable_missing_tool_ids_before_rig_decodes_it() {
        let line = br#"data: {"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{}}},{"functionCall":{"name":"read","args":{}}},{"functionCall":{"id":"upstream","name":"exec","args":{}}}]}}]}}
"#;
        let transformed = transform_sse_line(line, true).unwrap();
        let response: Value = serde_json::from_slice(
            transformed
                .strip_prefix(b"data: ")
                .unwrap()
                .strip_suffix(b"\n")
                .unwrap(),
        )
        .unwrap();
        let parts = &response["candidates"][0]["content"]["parts"];
        assert_eq!(parts[0]["functionCall"]["id"], "call_read_0");
        assert_eq!(parts[1]["functionCall"]["id"], "call_read_1");
        assert_eq!(parts[2]["functionCall"]["id"], "upstream");
    }

    #[test]
    fn signature_repair_and_session_hash_are_stable() {
        let mut request = json!({
            "contents": [{"parts": [{"thoughtSignature": "old"}]}]
        });
        assert!(repair_thought_signatures(&mut request));
        assert_eq!(
            request["contents"][0]["parts"][0]["thoughtSignature"],
            DUMMY_THOUGHT_SIGNATURE
        );
        assert_eq!(
            request["contents"][0]["parts"][0]["thought_signature"],
            DUMMY_THOUGHT_SIGNATURE
        );
        assert_eq!(stable_session_id("session"), stable_session_id("session"));
        assert!(stable_session_id("session").starts_with('-'));
    }

    #[test]
    fn inference_endpoints_follow_reference_fallback_order() {
        let endpoints = Endpoints {
            sandbox: "https://sandbox.test".to_string(),
            prod: "https://prod.test".to_string(),
            daily: "https://daily.test".to_string(),
        };
        assert_eq!(
            endpoints
                .inference_urls()
                .map(|url| url.strip_suffix(STREAM_PATH).unwrap().to_string()),
            [
                "https://sandbox.test".to_string(),
                "https://daily.test".to_string(),
                "https://prod.test".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn transport_refreshes_one_401_and_sends_private_envelope() {
        let (endpoint, captured) = mock_server(vec![
            (401, "application/json", r#"{"error":"expired"}"#),
            (
                200,
                "text/event-stream",
                "data: {\"response\":{\"candidates\":[]},\"responseId\":\"response-1\"}\n\n",
            ),
        ])
        .await;
        let refreshes = Arc::new(AtomicUsize::new(0));
        let transport = test_transport(endpoint, refreshes.clone(), "gemini-pro-agent");
        let response = transport
            .send(
                reqwest::Client::new(),
                test_request(json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]})),
            )
            .await
            .unwrap();
        assert_eq!(
            response_text(response).await,
            "data: {\"candidates\":[],\"responseId\":\"response-1\"}\n\n"
        );
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        let captured = captured.await.unwrap();
        assert_eq!(captured.len(), 2);
        assert!(
            captured[0]
                .head
                .starts_with("POST /v1internal:streamGenerateContent?alt=sse HTTP/1.1")
        );
        assert!(captured[0].head.contains("authorization: Bearer old-token"));
        assert!(captured[1].head.contains("authorization: Bearer new-token"));
        assert!(captured[0].head.contains("x-client-name: antigravity"));
        assert!(captured[0].head.contains("x-client-version: 4.3.0"));
        assert!(captured[0].head.contains("x-vscode-sessionid:"));
        assert!(captured[0].head.contains("x-goog-user-project: project-1"));
        assert!(captured[0].head.contains("transfer-encoding: chunked"));
        assert!(!captured[0].head.contains("content-length:"));
        assert!(
            captured[1]
                .head
                .contains(&format!("user-agent: {}", default_generation_user_agent()))
        );
        assert_eq!(captured[0].body["project"], "project-1");
        assert_eq!(captured[0].body["model"], "gemini-pro-agent");
        assert_eq!(captured[0].body["request"]["sessionId"], "-123");
        assert_eq!(captured[0].body["requestType"], "agent");
        assert_eq!(captured[0].body["userAgent"], "antigravity");
        assert_eq!(
            captured[0].body["enabledCreditTypes"],
            json!(["GOOGLE_ONE_AI"])
        );
        assert!(
            captured[0].body["requestId"]
                .as_str()
                .is_some_and(|id| id.starts_with("agent/") && id.split('/').count() == 3)
        );
    }

    #[tokio::test]
    async fn transport_falls_back_across_each_endpoint_on_server_errors() {
        let unavailable = || {
            (
                503,
                "application/json",
                r#"{"error":{"message":"capacity"}}"#,
            )
        };
        let (endpoint, captured) =
            mock_server(vec![unavailable(), unavailable(), unavailable()]).await;
        let refreshes = Arc::new(AtomicUsize::new(0));
        let result = test_transport(endpoint, refreshes.clone(), "gemini-pro-agent")
            .send(
                reqwest::Client::new(),
                test_request(json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]})),
            )
            .await;
        let Err(error) = result else {
            panic!("503 response unexpectedly succeeded")
        };
        assert!(matches!(
            error,
            rig::http_client::Error::InvalidStatusCodeWithDetails {
                status: StatusCode::SERVICE_UNAVAILABLE,
                ..
            }
        ));
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(captured.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn transport_retries_one_403_without_the_project_header() {
        let (endpoint, captured) = mock_server(vec![
            (403, "application/json", r#"{"error":"project denied"}"#),
            (
                200,
                "text/event-stream",
                "data: {\"response\":{\"candidates\":[]}}\n\n",
            ),
        ])
        .await;
        let response = test_transport(
            endpoint,
            Arc::new(AtomicUsize::new(0)),
            "gemini-3.7-flash-high",
        )
        .send(
            reqwest::Client::new(),
            test_request(json!({"contents": [{"parts": [{"text": "hi"}]}]})),
        )
        .await
        .unwrap();
        assert!(response_text(response).await.contains("candidates"));
        let captured = captured.await.unwrap();
        assert!(captured[0].head.contains("x-goog-user-project: project-1"));
        assert!(!captured[1].head.contains("x-goog-user-project:"));
    }

    #[tokio::test]
    async fn transport_repairs_a_gemini_signature_only_after_matching_400() {
        let (endpoint, captured) = mock_server(vec![
            (
                400,
                "application/json",
                r#"{"error":{"message":"invalid thought signature"}}"#,
            ),
            (
                200,
                "text/event-stream",
                "data: {\"response\":{\"candidates\":[]}}\n\n",
            ),
        ])
        .await;
        let transport = test_transport(endpoint, Arc::new(AtomicUsize::new(0)), "gemini-3-flash");
        let response = transport
            .send(
                reqwest::Client::new(),
                test_request(json!({
                    "contents": [{"role": "model", "parts": [{
                        "text": "prior", "thoughtSignature": "invalid-signature"
                    }]}]
                })),
            )
            .await
            .unwrap();
        assert!(response_text(response).await.contains("candidates"));
        let captured = captured.await.unwrap();
        assert_eq!(
            captured[0].body["request"]["contents"][0]["parts"][0]["thoughtSignature"],
            "invalid-signature"
        );
        assert_eq!(
            captured[1].body["request"]["contents"][0]["parts"][0]["thoughtSignature"],
            DUMMY_THOUGHT_SIGNATURE
        );
    }

    #[test]
    fn request_parts_mirror_signatures_and_fill_flash_and_claude_fields() {
        let mut flash = json!({
            "contents": [{"parts": [
                {"functionCall": {"name": "read", "args": {}}},
                {"functionCall": {"name": "exec", "args": {}}, "thought_signature": "real"}
            ]}]
        });
        prepare_inner_request(&mut flash, "-1", "gemini-3.7-flash-low", None).unwrap();
        assert_eq!(
            flash["contents"][0]["parts"][0]["thoughtSignature"],
            DUMMY_THOUGHT_SIGNATURE
        );
        assert_eq!(flash["contents"][0]["parts"][1]["thoughtSignature"], "real");

        let mut claude = json!({
            "contents": [
                {"role": "model", "parts": [
                    {"functionCall": {"name": "read", "args": {}}}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "read", "response": {}}}
                ]}
            ]
        });
        prepare_inner_request(&mut claude, "-1", "claude-sonnet-4-6", None).unwrap();
        assert_eq!(
            claude["contents"][0]["parts"][0]["functionCall"]["id"],
            "call_read_0"
        );
        assert_eq!(
            claude["contents"][1]["parts"][0]["functionResponse"]["id"],
            "call_read_0"
        );
    }

    #[tokio::test]
    async fn sse_unwrapper_handles_json_split_across_chunks() {
        let source: SourceStream = Box::pin(stream::iter(vec![
            Ok(Bytes::from_static(b"data: {\"response\":{\"candi")),
            Ok(Bytes::from_static(b"dates\":[]},\"responseId\":\"r1\"}\r")),
            Ok(Bytes::from_static(b"\n\r\n")),
        ]));
        let mut output = unwrap_sse_stream(source, false);
        let mut bytes = Vec::new();
        while let Some(chunk) = output.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "data: {\"candidates\":[],\"responseId\":\"r1\"}\r\n\r\n"
        );
    }
}
