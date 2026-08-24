use crate::catalog::CatalogModel;
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
use std::sync::Arc;

const PROVIDER: &str = "antigravity";
const PROD_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
const DAILY_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
const STREAM_PATH: &str = "/v1internal:streamGenerateContent?alt=sse";
const DUMMY_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";
const MAX_SSE_LINE_BYTES: usize = 8 * 1024 * 1024;

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
    upstream_model: String,
    session_id: String,
    identity_prompt: Option<String>,
    endpoints: Endpoints,
}

impl std::fmt::Debug for AntigravityTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AntigravityTransport")
            .field("upstream_model", &self.upstream_model)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl AntigravityTransport {
    pub(super) fn new(
        model: &CatalogModel,
        session_id: Option<&str>,
        manager: Arc<ConfigManager>,
    ) -> Result<Self> {
        let upstream_model = model
            .compat("antigravityModel")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "Antigravity model {}/{} has no compat.antigravityModel mapping",
                    model.provider,
                    model.id
                )
            })?
            .to_string();
        let identity_prompt = env::var("ANTIGRAVITY_IDENTITY_PROMPT")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let affinity = session_id
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        Ok(Self {
            credentials: Arc::new(ConfigCredentials { manager }),
            upstream_model,
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
        let mut inner = inner;

        loop {
            let body = self
                .envelope(inner.clone(), &token)
                .map_err(instance_error)?;
            let endpoint = self.endpoints.inference_url(&token);
            let response = send_once(&client, &request, &token, endpoint, body)
                .await
                .map_err(instance_error)?;
            if response.status().is_success() {
                return streaming_response(response);
            }

            let status = response.status();
            let headers = response.headers().clone();
            let error_body = response.text().await.unwrap_or_default();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                token = self.credentials.refresh().await.map_err(instance_error)?;
                refreshed = true;
                continue;
            }
            if status == StatusCode::BAD_REQUEST
                && !signature_repaired
                && self.upstream_model.starts_with("gemini-")
                && signature_error(&error_body)
                && replace_thought_signatures(&mut inner)
            {
                signature_repaired = true;
                continue;
            }
            return Err(rig::http_client::Error::InvalidStatusCodeWithDetails {
                status,
                body: error_body,
                headers: Box::new(headers),
            });
        }
    }

    fn envelope(&self, mut request: Value, token: &OauthToken) -> Result<Bytes> {
        prepare_inner_request(
            &mut request,
            &self.session_id,
            self.identity_prompt.as_deref(),
        )?;
        let project = extra_string(token, "projectId")
            .ok_or_else(|| anyhow!("Antigravity OAuth credential has no projectId"))?;
        Ok(Bytes::from(serde_json::to_vec(&json!({
            "project": project,
            "requestId": format!("agent-{}", uuid::Uuid::now_v7()),
            "userAgent": "antigravity",
            "requestType": "agent",
            "model": self.upstream_model,
            "request": request
        }))?))
    }
}

#[derive(Clone, Debug)]
struct Endpoints {
    prod: String,
    daily: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            prod: PROD_BASE_URL.to_string(),
            daily: DAILY_BASE_URL.to_string(),
        }
    }
}

impl Endpoints {
    fn inference_url(&self, token: &OauthToken) -> String {
        let tier = extra_string(token, "tier")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let base = if tier.contains("pro") || tier.contains("ultra") {
            &self.daily
        } else {
            &self.prod
        };
        format!("{}{STREAM_PATH}", base.trim_end_matches('/'))
    }
}

async fn send_once(
    client: &reqwest::Client,
    original: &Request<Bytes>,
    token: &OauthToken,
    endpoint: String,
    body: Bytes,
) -> Result<reqwest::Response> {
    let user_agent = extra_string(token, "userAgent")
        .or_else(|| env::var("ANTIGRAVITY_USER_AGENT").ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Antigravity OAuth credential has no userAgent"))?;
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
    let response = client
        .request(original.method().clone(), endpoint)
        .headers(headers)
        .body(body)
        .send()
        .await?;
    Ok(response)
}

fn streaming_response(
    response: reqwest::Response,
) -> rig::http_client::Result<rig::http_client::StreamingResponse> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let source: SourceStream = Box::pin(response.bytes_stream());
    let body = unwrap_sse_stream(source);
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
}

fn unwrap_sse_stream(source: SourceStream) -> BoxedStream {
    let state = SseState {
        source,
        buffer: Vec::new(),
        eof: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(line) = take_line(&mut state.buffer, state.eof) {
                let transformed = transform_sse_line(&line)
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

fn transform_sse_line(line: &[u8]) -> Result<Vec<u8>> {
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
    let mut envelope: Value = serde_json::from_slice(payload)
        .context("Antigravity SSE data is not a JSON response envelope")?;
    let Some(object) = envelope.as_object_mut() else {
        bail!("Antigravity SSE data is not an object")
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
    }
    let mut transformed = b"data: ".to_vec();
    transformed.extend_from_slice(&serde_json::to_vec(&response)?);
    transformed.extend_from_slice(ending);
    Ok(transformed)
}

fn prepare_inner_request(
    request: &mut Value,
    session_id: &str,
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
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if has_tools {
        let tool_config = body
            .entry("toolConfig")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(tool_config) = tool_config.as_object_mut() else {
            bail!("Antigravity toolConfig is not an object")
        };
        tool_config.insert(
            "functionCallingConfig".to_string(),
            json!({"mode": "VALIDATED"}),
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
    for declaration in tools
        .iter_mut()
        .filter_map(|tool| tool.get_mut("functionDeclarations"))
        .filter_map(Value::as_array_mut)
        .flatten()
        .filter_map(Value::as_object_mut)
    {
        if let Some(parameters) = declaration.get_mut("parameters") {
            clean_schema(parameters);
        }
    }
}

fn clean_schema(schema: &mut Value) {
    let definitions = schema
        .get("$defs")
        .or_else(|| schema.get("definitions"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    clean_schema_node(schema, &definitions, 0);
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
        && let Some(name) = reference.rsplit('/').next()
        && let Some(mut resolved) = definitions.get(name).cloned()
    {
        clean_schema_node(&mut resolved, definitions, depth + 1);
        if let Some(resolved) = resolved.as_object_mut() {
            resolved.append(object);
            *object = std::mem::take(resolved);
        }
    }
    for union in ["allOf", "anyOf", "oneOf"] {
        let Some(branches) = object
            .remove(union)
            .and_then(|value| value.as_array().cloned())
        else {
            continue;
        };
        for mut branch in branches {
            clean_schema_node(&mut branch, definitions, depth + 1);
            let Some(branch) = branch.as_object_mut() else {
                continue;
            };
            if branch.get("type").and_then(Value::as_str) == Some("null") {
                continue;
            }
            merge_schema_object(object, branch);
            if union != "allOf" {
                break;
            }
        }
    }
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for property in properties.values_mut() {
            clean_schema_node(property, definitions, depth + 1);
        }
    }
    if let Some(items) = object.get_mut("items") {
        if let Some(tuple) = items.as_array_mut() {
            *items = tuple
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) != Some("null"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "string"}));
        }
        clean_schema_node(items, definitions, depth + 1);
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
    let property_names = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect::<Vec<_>>());
    if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut)
        && let Some(property_names) = property_names
    {
        required.retain(|name| {
            name.as_str()
                .is_some_and(|name| property_names.iter().any(|property| property == name))
        });
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

fn replace_thought_signatures(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Object(object) => {
            if let Some(signature) = object.get_mut("thoughtSignature")
                && !signature.as_str().is_some_and(|value| value.is_empty())
            {
                *signature = Value::String(DUMMY_THOUGHT_SIGNATURE.to_string());
                changed = true;
            }
            for value in object.values_mut() {
                changed |= replace_thought_signatures(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                changed |= replace_thought_signatures(value);
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
                let (header_end, content_length) = loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let header_end = header_end + 4;
                        let head = String::from_utf8_lossy(&bytes[..header_end]);
                        let content_length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or_default();
                        if bytes.len() >= header_end + content_length {
                            break (header_end, content_length);
                        }
                    }
                };
                captured.push(CapturedRequest {
                    head: String::from_utf8_lossy(&bytes[..header_end]).into_owned(),
                    body: serde_json::from_slice(&bytes[header_end..header_end + content_length])
                        .unwrap(),
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
                (
                    "userAgent".to_string(),
                    Value::String("antigravity/1.23.2 windows/amd64".to_string()),
                ),
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
            upstream_model: upstream_model.to_string(),
            session_id: "-123".to_string(),
            identity_prompt: None,
            endpoints: Endpoints {
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
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"uri": {"type": "string", "minLength": 1}},
                    "required": ["uri", "missing"]
                }
            }]}]
        });
        prepare_inner_request(&mut request, "-123", Some("explicit identity")).unwrap();
        assert_eq!(request["sessionId"], "-123");
        assert_eq!(request["contents"].as_array().unwrap().len(), 1);
        assert_eq!(
            request["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
        let parameters = &request["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(parameters.get("additionalProperties").is_none());
        assert_eq!(parameters["required"], json!(["uri"]));
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
    }

    #[test]
    fn wrapped_sse_is_unwrapped_across_line_endings() {
        let line = b"data: {\"response\":{\"candidates\":[]},\"responseId\":\"r1\"}\r\n";
        assert_eq!(
            String::from_utf8(transform_sse_line(line).unwrap()).unwrap(),
            "data: {\"candidates\":[],\"responseId\":\"r1\"}\r\n"
        );
        assert_eq!(transform_sse_line(b": ping\n").unwrap(), b": ping\n");
        assert!(transform_sse_line(b"data: not-json\n").is_err());
    }

    #[test]
    fn signature_repair_and_session_hash_are_stable() {
        let mut request = json!({
            "contents": [{"parts": [{"thoughtSignature": "old"}]}]
        });
        assert!(replace_thought_signatures(&mut request));
        assert_eq!(
            request["contents"][0]["parts"][0]["thoughtSignature"],
            DUMMY_THOUGHT_SIGNATURE
        );
        assert_eq!(stable_session_id("session"), stable_session_id("session"));
        assert!(stable_session_id("session").starts_with('-'));
    }

    #[test]
    fn paid_tiers_use_daily_and_free_tiers_use_production() {
        let endpoints = Endpoints {
            prod: "https://prod.test".to_string(),
            daily: "https://daily.test".to_string(),
        };
        let token = |tier: &str| OauthToken {
            kind: "oauth".to_string(),
            refresh: "refresh".to_string(),
            access: "access".to_string(),
            expires: i64::MAX,
            extra: BTreeMap::from([("tier".to_string(), Value::String(tier.to_string()))]),
        };
        assert!(
            endpoints
                .inference_url(&token("g1-pro-tier"))
                .starts_with("https://daily.test")
        );
        assert!(
            endpoints
                .inference_url(&token("free-tier"))
                .starts_with("https://prod.test")
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
        assert!(
            captured[1]
                .head
                .contains("user-agent: antigravity/1.23.2 windows/amd64")
        );
        assert_eq!(captured[0].body["project"], "project-1");
        assert_eq!(captured[0].body["model"], "gemini-pro-agent");
        assert_eq!(captured[0].body["request"]["sessionId"], "-123");
        assert_eq!(captured[0].body["requestType"], "agent");
        assert_eq!(captured[0].body["userAgent"], "antigravity");
    }

    #[tokio::test]
    async fn transport_does_not_replay_server_errors() {
        let (endpoint, captured) = mock_server(vec![(
            503,
            "application/json",
            r#"{"error":{"message":"capacity"}}"#,
        )])
        .await;
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
        assert_eq!(captured.await.unwrap().len(), 1);
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

    #[tokio::test]
    async fn sse_unwrapper_handles_json_split_across_chunks() {
        let source: SourceStream = Box::pin(stream::iter(vec![
            Ok(Bytes::from_static(b"data: {\"response\":{\"candi")),
            Ok(Bytes::from_static(b"dates\":[]},\"responseId\":\"r1\"}\r")),
            Ok(Bytes::from_static(b"\n\r\n")),
        ]));
        let mut output = unwrap_sse_stream(source);
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
