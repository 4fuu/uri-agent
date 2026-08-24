use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, Request, Response, Uri};
use rig::http_client::{self, HttpClientExt};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_IDLE_TTL: Duration = Duration::from_secs(5 * 60);
const CONNECTION_MAX_AGE: Duration = Duration::from_secs(55 * 60);
const WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
const CONNECTION_LIMIT_REACHED: &str = "websocket_connection_limit_reached";
const PREVIOUS_RESPONSE_NOT_FOUND: &str = "previous_response_not_found";

type CodexSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Eq, Hash, PartialEq)]
struct CacheKey {
    session_id: String,
    account_id: String,
}

static SESSION_CACHE: LazyLock<StdMutex<HashMap<CacheKey, Weak<Mutex<TransportState>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));
static SESSION_FALLBACK: LazyLock<StdMutex<HashMap<String, Weak<AtomicBool>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

#[derive(Clone)]
pub(crate) struct CodexWebSocketTransport {
    state: Arc<Mutex<TransportState>>,
    session_fallback: Option<Arc<AtomicBool>>,
    cache_enabled: bool,
}

impl fmt::Debug for CodexWebSocketTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexWebSocketTransport")
            .field("cache_enabled", &self.cache_enabled)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct TransportState {
    cached: Option<CachedConnection>,
    websocket_disabled: bool,
}

struct CachedConnection {
    socket: Arc<Mutex<CodexSocket>>,
    busy: bool,
    created_at: Instant,
    last_used: Instant,
    continuation: Option<Continuation>,
}

#[derive(Clone)]
struct Continuation {
    last_request_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

struct ConnectionLease {
    socket: Arc<Mutex<CodexSocket>>,
    cached: bool,
    request_body: Value,
    full_request_body: Value,
    stale_response_id: Option<String>,
}

#[derive(Default)]
struct EventMetadata {
    forward_to_model: bool,
    terminal: bool,
    successful_terminal: bool,
    error_code: Option<String>,
    response_id: Option<String>,
    output_item: Option<(u64, Value)>,
    terminal_output: Option<Vec<Value>>,
    malformed: bool,
}

impl CodexWebSocketTransport {
    pub(crate) fn new(cache_scope: Option<(&str, &str)>) -> Self {
        let Some((session_id, account_id)) = cache_scope else {
            return Self {
                state: Arc::new(Mutex::new(TransportState::default())),
                session_fallback: None,
                cache_enabled: false,
            };
        };
        let key = CacheKey {
            session_id: session_id.to_string(),
            account_id: account_id.to_string(),
        };
        let mut cache = SESSION_CACHE.lock().expect("Codex session cache poisoned");
        cache.retain(|_, state| state.strong_count() > 0);
        let state = cache.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let state = Arc::new(Mutex::new(TransportState::default()));
            cache.insert(key, Arc::downgrade(&state));
            state
        });
        drop(cache);
        let mut fallbacks = SESSION_FALLBACK
            .lock()
            .expect("Codex session fallback cache poisoned");
        fallbacks.retain(|_, fallback| fallback.strong_count() > 0);
        let session_fallback = fallbacks
            .get(session_id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let fallback = Arc::new(AtomicBool::new(false));
                fallbacks.insert(session_id.to_string(), Arc::downgrade(&fallback));
                fallback
            });
        Self {
            state,
            session_fallback: Some(session_fallback),
            cache_enabled: true,
        }
    }

    pub(crate) async fn send(
        &self,
        client: reqwest::Client,
        request: Request<Bytes>,
    ) -> http_client::Result<http_client::StreamingResponse> {
        let Some(full_request_body) = request_json(request.body()) else {
            return HttpClientExt::send_streaming(&client, request).await;
        };
        if self.websocket_disabled() || self.state.lock().await.websocket_disabled {
            return HttpClientExt::send_streaming(&client, request).await;
        }

        let websocket_uri = websocket_uri(request.uri()).map_err(instance_error)?;
        let websocket_headers = websocket_headers(request.headers());
        let mut force_full = false;
        let mut retried_connection_limit = false;
        let mut retried_missing_continuation = false;

        loop {
            let lease = match self
                .acquire(
                    &websocket_uri,
                    &websocket_headers,
                    &full_request_body,
                    force_full,
                )
                .await
            {
                Ok(lease) => lease,
                Err(_) => {
                    self.disable_websocket().await;
                    return HttpClientExt::send_streaming(&client, request).await;
                }
            };

            if let Err(_error) = send_response_create(&lease.socket, &lease.request_body).await {
                self.release(lease, None, false).await;
                self.disable_websocket().await;
                return HttpClientExt::send_streaming(&client, request).await;
            }

            let mut saw_diagnostic_event = false;
            let (first_payload, first_metadata) = loop {
                let first_payload =
                    match read_payload(&lease.socket, lease.stale_response_id.as_deref()).await {
                        Ok(payload) => payload,
                        Err(error) if saw_diagnostic_event => {
                            self.disable_websocket().await;
                            self.release(lease, None, false).await;
                            return streaming_response(vec![Err(instance_error(error))]);
                        }
                        Err(_) => {
                            self.release(lease, None, false).await;
                            self.disable_websocket().await;
                            return HttpClientExt::send_streaming(&client, request).await;
                        }
                    };
                let inspected = inspect_payload(first_payload);
                if inspected.1.forward_to_model {
                    break inspected;
                }
                saw_diagnostic_event = true;
            };

            let missing_continuation = first_metadata.error_code.as_deref()
                == Some(PREVIOUS_RESPONSE_NOT_FOUND)
                && !retried_missing_continuation;
            let connection_limit = first_metadata.error_code.as_deref()
                == Some(CONNECTION_LIMIT_REACHED)
                && !saw_diagnostic_event
                && !retried_connection_limit;
            if missing_continuation || connection_limit {
                if missing_continuation {
                    retried_missing_continuation = true;
                } else {
                    retried_connection_limit = true;
                }
                force_full = true;
                self.release(lease, None, false).await;
                continue;
            }

            return self
                .stream_from_first(lease, first_payload, first_metadata)
                .await;
        }
    }

    async fn acquire(
        &self,
        websocket_uri: &str,
        headers: &HeaderMap,
        full_request_body: &Value,
        force_full: bool,
    ) -> Result<ConnectionLease, String> {
        let now = Instant::now();
        let mut expired = None;
        if self.cache_enabled {
            let mut state = self.state.lock().await;
            if self.websocket_disabled() || state.websocket_disabled {
                return Err("WebSocket transport is disabled for this session".to_string());
            }
            if let Some(cached) = state.cached.as_mut() {
                let expired_by_age = now.duration_since(cached.created_at) >= CONNECTION_MAX_AGE;
                let expired_by_idle =
                    !cached.busy && now.duration_since(cached.last_used) >= CACHE_IDLE_TTL;
                if !cached.busy && (expired_by_age || expired_by_idle) {
                    expired = state.cached.take().map(|cached| cached.socket);
                } else if !cached.busy {
                    cached.busy = true;
                    let stale_response_id = cached
                        .continuation
                        .as_ref()
                        .map(|continuation| continuation.last_response_id.clone());
                    let request_body = if force_full {
                        cached.continuation = None;
                        full_request_body.clone()
                    } else {
                        cached_request_body(full_request_body, &mut cached.continuation)
                    };
                    return Ok(ConnectionLease {
                        socket: cached.socket.clone(),
                        cached: true,
                        request_body,
                        full_request_body: full_request_body.clone(),
                        stale_response_id,
                    });
                }
            }
        }
        if let Some(socket) = expired {
            close_socket(socket).await;
        }

        let socket = Arc::new(Mutex::new(connect(websocket_uri, headers).await?));
        if !self.cache_enabled {
            return Ok(ConnectionLease {
                socket,
                cached: false,
                request_body: full_request_body.clone(),
                full_request_body: full_request_body.clone(),
                stale_response_id: None,
            });
        }

        let mut state = self.state.lock().await;
        if self.websocket_disabled() || state.websocket_disabled {
            drop(state);
            close_socket(socket).await;
            return Err("WebSocket transport is disabled for this session".to_string());
        }
        if state.cached.is_none() {
            state.cached = Some(CachedConnection {
                socket: socket.clone(),
                busy: true,
                created_at: now,
                last_used: now,
                continuation: None,
            });
            return Ok(ConnectionLease {
                socket,
                cached: true,
                request_body: full_request_body.clone(),
                full_request_body: full_request_body.clone(),
                stale_response_id: None,
            });
        }

        // The cached connection is busy with another turn. Codex allows only
        // one response in flight per socket, so this request uses a one-shot
        // connection and leaves the session cache untouched.
        Ok(ConnectionLease {
            socket,
            cached: false,
            request_body: full_request_body.clone(),
            full_request_body: full_request_body.clone(),
            stale_response_id: None,
        })
    }

    async fn stream_from_first(
        &self,
        lease: ConnectionLease,
        first_payload: String,
        first_metadata: EventMetadata,
    ) -> http_client::Result<http_client::StreamingResponse> {
        let first_chunk = sse_chunk(&first_payload);
        if first_metadata.terminal {
            let continuation = continuation_from_terminal(
                &lease.full_request_body,
                &first_metadata,
                BTreeMap::new(),
            );
            let keep = first_metadata.successful_terminal;
            self.release(lease, continuation, keep).await;
            return streaming_response(vec![Ok(first_chunk), Ok(done_chunk())]);
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = sender.send(Ok(first_chunk));
        let transport = self.clone();
        tokio::spawn(async move {
            transport
                .forward_remaining(lease, sender, first_metadata)
                .await;
        });
        streaming_response_from_receiver(receiver)
    }

    async fn forward_remaining(
        &self,
        lease: ConnectionLease,
        sender: mpsc::UnboundedSender<http_client::Result<Bytes>>,
        first_metadata: EventMetadata,
    ) {
        let mut output_items = BTreeMap::new();
        record_output_item(&mut output_items, &first_metadata);
        loop {
            let payload = tokio::select! {
                _ = sender.closed() => {
                    self.release(lease, None, false).await;
                    return;
                }
                result = read_payload(&lease.socket, None) => result,
            };
            let payload = match payload {
                Ok(payload) => payload,
                Err(error) => {
                    self.disable_websocket().await;
                    let _ = sender.send(Err(instance_error(error)));
                    self.release(lease, None, false).await;
                    return;
                }
            };
            let (payload, metadata) = inspect_payload(payload);
            record_output_item(&mut output_items, &metadata);
            if sender.send(Ok(sse_chunk(&payload))).is_err() {
                self.release(lease, None, false).await;
                return;
            }
            if metadata.terminal {
                let continuation =
                    continuation_from_terminal(&lease.full_request_body, &metadata, output_items);
                let keep = metadata.successful_terminal;
                self.release(lease, continuation, keep).await;
                let _ = sender.send(Ok(done_chunk()));
                return;
            }
        }
    }

    async fn release(
        &self,
        lease: ConnectionLease,
        continuation: Option<Continuation>,
        keep: bool,
    ) {
        if !lease.cached {
            close_socket(lease.socket).await;
            return;
        }

        let mut close = None;
        let mut schedule_expiry = false;
        let socket = lease.socket.clone();
        {
            let mut state = self.state.lock().await;
            let matches_cached = state
                .cached
                .as_ref()
                .is_some_and(|cached| Arc::ptr_eq(&cached.socket, &lease.socket));
            if matches_cached && keep && !self.websocket_disabled() && !state.websocket_disabled {
                if let Some(cached) = state.cached.as_mut() {
                    cached.busy = false;
                    cached.last_used = Instant::now();
                    cached.continuation = continuation;
                    schedule_expiry = true;
                }
            } else if matches_cached {
                close = state.cached.take().map(|cached| cached.socket);
            } else if !keep {
                close = Some(socket.clone());
            }
        }
        if let Some(socket) = close {
            close_socket(socket).await;
        } else if schedule_expiry {
            schedule_idle_expiry(Arc::downgrade(&self.state), socket);
        }
    }

    async fn disable_websocket(&self) {
        if let Some(fallback) = &self.session_fallback {
            fallback.store(true, Ordering::Release);
        }
        let socket = {
            let mut state = self.state.lock().await;
            state.websocket_disabled = true;
            state.cached.take().map(|cached| cached.socket)
        };
        if let Some(socket) = socket {
            close_socket(socket).await;
        }
    }

    fn websocket_disabled(&self) -> bool {
        self.session_fallback
            .as_ref()
            .is_some_and(|fallback| fallback.load(Ordering::Acquire))
    }
}

fn request_json(body: &Bytes) -> Option<Value> {
    let mut body = serde_json::from_slice::<Value>(body).ok()?;
    let object = body.as_object_mut()?;
    object.insert("store".to_string(), Value::Bool(false));
    object.remove("previous_response_id");
    Some(body)
}

fn cached_request_body(full_body: &Value, continuation: &mut Option<Continuation>) -> Value {
    let Some(cached) = continuation.as_ref() else {
        return full_body.clone();
    };
    if body_without_input(full_body) != body_without_input(&cached.last_request_body) {
        *continuation = None;
        return full_body.clone();
    }
    let Some(current_input) = full_body.get("input").and_then(Value::as_array) else {
        *continuation = None;
        return full_body.clone();
    };
    let previous_input = cached
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut baseline = previous_input;
    baseline.extend(cached.last_response_items.iter().cloned());
    if current_input.len() < baseline.len()
        || canonical_items(&current_input[..baseline.len()]) != canonical_items(&baseline)
    {
        *continuation = None;
        return full_body.clone();
    }

    let mut request = full_body.clone();
    let Some(object) = request.as_object_mut() else {
        return full_body.clone();
    };
    object.insert(
        "previous_response_id".to_string(),
        Value::String(cached.last_response_id.clone()),
    );
    object.insert(
        "input".to_string(),
        Value::Array(current_input[baseline.len()..].to_vec()),
    );
    request
}

fn body_without_input(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("input");
        object.remove("previous_response_id");
    }
    body
}

fn canonical_items(items: &[Value]) -> Vec<Value> {
    items.iter().cloned().map(canonical_value).collect()
}

fn canonical_value(mut value: Value) -> Value {
    match &mut value {
        Value::Array(items) => {
            for item in items {
                *item = canonical_value(item.take());
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                *value = canonical_value(value.take());
            }
            object.retain(|key, value| {
                !value.is_null()
                    && !((key == "annotations" || key == "logprobs")
                        && value.as_array().is_some_and(Vec::is_empty))
            });
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    value
}

fn websocket_uri(uri: &Uri) -> Result<String, String> {
    let uri = uri.to_string();
    if let Some(rest) = uri.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = uri.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else {
        Err(format!("unsupported Codex WebSocket URL: {uri}"))
    }
}

fn websocket_headers(headers: &HeaderMap) -> HeaderMap {
    let mut websocket = headers.clone();
    for name in [
        http::header::ACCEPT,
        http::header::CONTENT_TYPE,
        http::header::CONTENT_LENGTH,
        http::header::HOST,
        http::header::CONNECTION,
        http::header::UPGRADE,
    ] {
        websocket.remove(name);
    }
    websocket.insert(
        http::HeaderName::from_static("openai-beta"),
        HeaderValue::from_static(WEBSOCKET_BETA),
    );
    let request_id = websocket
        .get("x-client-request-id")
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_str(&uuid::Uuid::now_v7().to_string()).unwrap());
    websocket.insert("x-client-request-id", request_id.clone());
    websocket.insert("session-id", request_id);
    websocket
}

async fn connect(uri: &str, headers: &HeaderMap) -> Result<CodexSocket, String> {
    let mut request = uri
        .into_client_request()
        .map_err(|error| format!("cannot build Codex WebSocket request: {error}"))?;
    for (name, value) in headers {
        request.headers_mut().insert(name, value.clone());
    }
    match tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request)).await {
        Ok(Ok((socket, _response))) => Ok(socket),
        Ok(Err(error)) => Err(format!("cannot connect Codex WebSocket: {error}")),
        Err(_) => Err(format!(
            "Codex WebSocket connect timeout after {} seconds",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

async fn send_response_create(
    socket: &Arc<Mutex<CodexSocket>>,
    body: &Value,
) -> Result<(), String> {
    let Some(fields) = body.as_object() else {
        return Err("Codex WebSocket request body is not an object".to_string());
    };
    let mut event = Map::new();
    event.insert(
        "type".to_string(),
        Value::String("response.create".to_string()),
    );
    event.extend(fields.clone());
    socket
        .lock()
        .await
        .send(Message::Text(Value::Object(event).to_string().into()))
        .await
        .map_err(|error| format!("cannot send Codex WebSocket request: {error}"))
}

async fn read_payload(
    socket: &Arc<Mutex<CodexSocket>>,
    stale_response_id: Option<&str>,
) -> Result<String, String> {
    loop {
        let message = socket
            .lock()
            .await
            .next()
            .await
            .ok_or_else(|| "Codex WebSocket closed before the response completed".to_string())?
            .map_err(|error| format!("Codex WebSocket receive failed: {error}"))?;
        let payload = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
                .map_err(|error| format!("Codex WebSocket returned non-UTF-8 data: {error}"))?,
            Message::Ping(payload) => {
                socket
                    .lock()
                    .await
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| format!("cannot answer Codex WebSocket ping: {error}"))?;
                continue;
            }
            Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(frame) => {
                let detail = frame.map_or_else(String::new, |frame| {
                    format!(" ({} {})", frame.code, frame.reason)
                });
                return Err(format!(
                    "Codex WebSocket closed before the response completed{detail}"
                ));
            }
        };
        if stale_response_id.is_some_and(|stale_id| is_stale_done(&payload, stale_id)) {
            continue;
        }
        return Ok(payload);
    }
}

fn is_stale_done(payload: &str, stale_response_id: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return false;
    };
    value.get("type").and_then(Value::as_str) == Some("response.done")
        && value
            .get("response")
            .and_then(|response| response.get("id"))
            .and_then(Value::as_str)
            == Some(stale_response_id)
}

fn inspect_payload(payload: String) -> (String, EventMetadata) {
    let Ok(mut value) = serde_json::from_str::<Value>(&payload) else {
        return (
            payload,
            EventMetadata {
                forward_to_model: true,
                terminal: true,
                malformed: true,
                ..EventMetadata::default()
            },
        );
    };
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let forward_to_model = kind == "error" || kind.starts_with("response.");
    if kind == "response.done"
        && let Some(object) = value.as_object_mut()
    {
        object.insert(
            "type".to_string(),
            Value::String("response.completed".to_string()),
        );
    }
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let response = value.get("response");
    let status = response
        .and_then(|response| response.get("status"))
        .and_then(Value::as_str);
    let terminal = matches!(
        kind,
        "error" | "response.completed" | "response.incomplete" | "response.failed"
    );
    let successful_terminal = matches!(kind, "response.completed" | "response.incomplete")
        && !matches!(status, Some("failed" | "cancelled"));
    let error_code = if kind == "error" {
        value
            .get("code")
            .or_else(|| value.get("error").and_then(|error| error.get("code")))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else if kind == "response.failed" {
        response
            .and_then(|response| response.get("error"))
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    let response_id = response
        .and_then(|response| response.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let output_item = (kind == "response.output_item.done")
        .then(|| {
            Some((
                value.get("output_index")?.as_u64()?,
                value.get("item")?.clone(),
            ))
        })
        .flatten();
    let terminal_output = terminal
        .then(|| {
            response?
                .get("output")?
                .as_array()
                .filter(|output| !output.is_empty())
                .cloned()
        })
        .flatten();
    (
        value.to_string(),
        EventMetadata {
            forward_to_model,
            terminal,
            successful_terminal,
            error_code,
            response_id,
            output_item,
            terminal_output,
            malformed: false,
        },
    )
}

fn record_output_item(output: &mut BTreeMap<u64, Value>, metadata: &EventMetadata) {
    if let Some((index, item)) = &metadata.output_item {
        output.insert(*index, item.clone());
    }
}

fn continuation_from_terminal(
    full_request_body: &Value,
    metadata: &EventMetadata,
    output_items: BTreeMap<u64, Value>,
) -> Option<Continuation> {
    if !metadata.successful_terminal || metadata.malformed {
        return None;
    }
    let response_id = metadata.response_id.clone()?;
    let output = metadata
        .terminal_output
        .clone()
        .unwrap_or_else(|| output_items.into_values().collect());
    let response_items = output
        .into_iter()
        .filter_map(response_item_for_replay)
        .collect();
    Some(Continuation {
        last_request_body: full_request_body.clone(),
        last_response_id: response_id,
        last_response_items: response_items,
    })
}

fn response_item_for_replay(item: Value) -> Option<Value> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call_output" | "custom_tool_call_output") => None,
        Some("message") => {
            let text = item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|content| {
                    matches!(
                        content.get("type").and_then(Value::as_str),
                        Some("output_text" | "refusal")
                    )
                    .then(|| {
                        content
                            .get("text")
                            .or_else(|| content.get("refusal"))
                            .and_then(Value::as_str)
                    })
                    .flatten()
                })
                .collect::<String>();
            (!text.is_empty())
                .then(|| json!({"type": "message", "role": "assistant", "content": text}))
        }
        Some(_) => Some(canonical_value(item)),
        None => None,
    }
}

fn sse_chunk(payload: &str) -> Bytes {
    Bytes::from(format!("data: {payload}\n\n"))
}

fn done_chunk() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

fn streaming_response(
    chunks: Vec<http_client::Result<Bytes>>,
) -> http_client::Result<http_client::StreamingResponse> {
    let stream: rig::http_client::sse::BoxedStream = Box::pin(futures_util::stream::iter(chunks));
    Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/event-stream")
        .body(stream)
        .map_err(http_client::Error::Protocol)
}

fn streaming_response_from_receiver(
    receiver: mpsc::UnboundedReceiver<http_client::Result<Bytes>>,
) -> http_client::Result<http_client::StreamingResponse> {
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    let stream: rig::http_client::sse::BoxedStream = Box::pin(stream);
    Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/event-stream")
        .body(stream)
        .map_err(http_client::Error::Protocol)
}

fn instance_error(error: impl Into<String>) -> http_client::Error {
    http_client::Error::Instance(Box::new(std::io::Error::other(error.into())))
}

fn schedule_idle_expiry(
    state: Weak<Mutex<TransportState>>,
    expected_socket: Arc<Mutex<CodexSocket>>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(CACHE_IDLE_TTL).await;
        let Some(state) = state.upgrade() else {
            return;
        };
        let socket = {
            let mut state = state.lock().await;
            let expired = state.cached.as_ref().is_some_and(|cached| {
                !cached.busy
                    && Arc::ptr_eq(&cached.socket, &expected_socket)
                    && cached.last_used.elapsed() >= CACHE_IDLE_TTL
            });
            expired
                .then(|| state.cached.take().map(|cached| cached.socket))
                .flatten()
        };
        if let Some(socket) = socket {
            close_socket(socket).await;
        }
    });
}

async fn close_socket(socket: Arc<Mutex<CodexSocket>>) {
    let _ = socket.lock().await.close(None).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_cache_is_shared_by_session_and_scoped_by_account() {
        let first = CodexWebSocketTransport::new(Some(("cache-scope-test", "account-a")));
        let resumed = CodexWebSocketTransport::new(Some(("cache-scope-test", "account-a")));
        let other_account = CodexWebSocketTransport::new(Some(("cache-scope-test", "account-b")));

        assert!(Arc::ptr_eq(&first.state, &resumed.state));
        assert!(!Arc::ptr_eq(&first.state, &other_account.state));
        assert!(Arc::ptr_eq(
            first.session_fallback.as_ref().unwrap(),
            other_account.session_fallback.as_ref().unwrap()
        ));
    }

    #[tokio::test]
    async fn websocket_fallback_applies_to_the_entire_session() {
        let failed = CodexWebSocketTransport::new(Some(("fallback-scope-test", "account-a")));
        let other_account =
            CodexWebSocketTransport::new(Some(("fallback-scope-test", "account-b")));

        failed.disable_websocket().await;

        assert!(other_account.websocket_disabled());
    }

    fn request(input: Vec<Value>) -> Value {
        json!({
            "model": "gpt-5.4",
            "store": false,
            "stream": true,
            "instructions": "system",
            "input": input,
            "tools": []
        })
    }

    #[test]
    fn cached_request_sends_only_input_after_the_previous_response() {
        let first = request(vec![json!({"role": "user", "content": "first"})]);
        let mut continuation = Some(Continuation {
            last_request_body: first.clone(),
            last_response_id: "resp_1".to_string(),
            last_response_items: vec![json!({"role": "assistant", "content": "answer"})],
        });
        let second = request(vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": "answer"}),
            json!({"type": "function_call_output", "call_id": "call_1", "output": "done"}),
            json!({"role": "user", "content": "second"}),
        ]);

        let cached = cached_request_body(&second, &mut continuation);

        assert_eq!(cached["previous_response_id"], "resp_1");
        assert_eq!(
            cached["input"],
            json!([
                {"type": "function_call_output", "call_id": "call_1", "output": "done"},
                {"role": "user", "content": "second"}
            ])
        );
        assert!(continuation.is_some());
    }

    #[test]
    fn cached_request_clears_continuation_when_context_or_options_change() {
        let first = request(vec![json!({"role": "user", "content": "first"})]);
        for second in [
            request(vec![json!({"role": "user", "content": "different"})]),
            {
                let mut changed = request(vec![
                    json!({"role": "user", "content": "first"}),
                    json!({"role": "assistant", "content": "answer"}),
                    json!({"role": "user", "content": "second"}),
                ]);
                changed["instructions"] = Value::String("changed".to_string());
                changed
            },
        ] {
            let mut continuation = Some(Continuation {
                last_request_body: first.clone(),
                last_response_id: "resp_1".to_string(),
                last_response_items: vec![json!({
                    "role": "assistant",
                    "content": "answer"
                })],
            });

            let cached = cached_request_body(&second, &mut continuation);

            assert_eq!(cached, second);
            assert!(continuation.is_none());
        }
    }

    #[test]
    fn response_output_is_normalized_to_uri_agent_history_replay() {
        let item = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [
                {"type": "output_text", "text": "hello", "annotations": []},
                {"type": "output_text", "text": " world", "annotations": []}
            ]
        });

        assert_eq!(
            response_item_for_replay(item),
            Some(json!({
                "type": "message",
                "role": "assistant",
                "content": "hello world"
            }))
        );
    }

    #[test]
    fn websocket_terminal_and_retry_errors_are_identified() {
        let (completed, metadata) = inspect_payload(
            json!({
                "type": "response.done",
                "response": {"id": "resp_1", "status": "completed", "output": []}
            })
            .to_string(),
        );
        assert_eq!(
            serde_json::from_str::<Value>(&completed).unwrap()["type"],
            "response.completed"
        );
        assert!(metadata.terminal);
        assert!(metadata.successful_terminal);
        assert_eq!(metadata.response_id.as_deref(), Some("resp_1"));

        for code in [PREVIOUS_RESPONSE_NOT_FOUND, CONNECTION_LIMIT_REACHED] {
            let (_, metadata) = inspect_payload(
                json!({"type": "error", "error": {"code": code, "message": "retry"}}).to_string(),
            );
            assert!(metadata.terminal);
            assert_eq!(metadata.error_code.as_deref(), Some(code));
        }
    }
}
