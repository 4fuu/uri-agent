//! Minimal HTTP ingress for the simple mode. A request either submits a
//! prompt and waits for its reply or injects a Steer message into an active
//! session. The session, context, and queue are the ordinary runtime ones;
//! this module only adapts text to `AgentHandle`.

use crate::agent::{AgentHandle, AgentHost, AgentStatus, SubmitKind};
use crate::config::Config;
use crate::execute;
use crate::session::Session;
use anyhow::{Context, Result, bail};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct AppState {
    host: AgentHost,
    config: Arc<Config>,
    sessions: Arc<Mutex<HashMap<String, AgentHandle>>>,
    token: Option<String>,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

fn api_error(error: anyhow::Error) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

fn authorize(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let Some(token) = &state.token else {
        return Ok(());
    };
    let authorized = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value == token);
    if authorized {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token".to_string(),
        ))
    }
}

pub async fn serve(host: AgentHost, config: Config, listen: &str) -> Result<()> {
    let token = std::env::var("UAT_HTTP_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let token_configured = token.is_some();
    let state = AppState {
        host,
        config: Arc::new(config),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        token,
    };
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/{id}", get(session_detail))
        .route("/v1/sessions/{id}/cancel", post(cancel))
        .route("/v1/messages", post(messages))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot bind {listen}"))?;
    let address = listener
        .local_addr()
        .context("cannot read server address")?;
    if !address.ip().is_loopback() && !token_configured {
        bail!("refusing to serve on non-loopback address {address} without UAT_HTTP_TOKEN");
    }
    eprintln!("uat serve listening on http://{address}");
    axum::serve(listener, app)
        .await
        .context("HTTP server failed")?;
    Ok(())
}

async fn health(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<&'static str> {
    authorize(&state, &headers)?;
    Ok("ok")
}

#[derive(Serialize)]
struct SessionEntry {
    session: String,
    status: &'static str,
}

async fn list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<SessionEntry>>> {
    authorize(&state, &headers)?;
    let handles = state
        .sessions
        .lock()
        .await
        .iter()
        .map(|(id, handle)| (id.clone(), handle.clone()))
        .collect::<Vec<_>>();
    let mut entries = Vec::with_capacity(handles.len());
    for (session, handle) in handles {
        let status = match handle.status().await {
            AgentStatus::Idle => "idle",
            AgentStatus::Running => "running",
            AgentStatus::Closed => "closed",
        };
        entries.push(SessionEntry { session, status });
    }
    Ok(Json(entries))
}

#[derive(Serialize)]
struct SessionDetail {
    session: String,
    status: &'static str,
    answer: Option<String>,
}

async fn session_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionDetail>> {
    authorize(&state, &headers)?;
    let handle = resolve_session(&state, Some(&id)).await?;
    let status = match handle.status().await {
        AgentStatus::Idle => "idle",
        AgentStatus::Running => "running",
        AgentStatus::Closed => "closed",
    };
    let answer = handle.result().await;
    Ok(Json(SessionDetail {
        session: handle.session_id().to_string(),
        status,
        answer,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageRequest {
    session: Option<String>,
    text: String,
    mode: Option<String>,
    wait: Option<bool>,
}

#[derive(Serialize)]
struct MessageResponse {
    session: String,
    accepted: bool,
    answer: Option<String>,
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MessageRequest>,
) -> ApiResult<Json<MessageResponse>> {
    authorize(&state, &headers)?;
    if request.text.trim().is_empty() {
        return Err(bad_request("text cannot be empty"));
    }
    let handle = resolve_session(&state, request.session.as_deref()).await?;
    let session = handle.session_id().to_string();
    match request.mode.as_deref().unwrap_or("prompt") {
        "steer" => {
            handle
                .submit(request.text, SubmitKind::Steer)
                .await
                .map_err(api_error)?;
            Ok(Json(MessageResponse {
                session,
                accepted: true,
                answer: None,
            }))
        }
        "prompt" => {
            if request.wait.unwrap_or(true) {
                let runtime = handle.services().runtime.clone();
                let updates = runtime.session().subscribe();
                let answer = execute::run_turn(
                    &runtime,
                    &request.text,
                    updates,
                    |_| {},
                    CancellationToken::new(),
                )
                .await
                .map_err(api_error)?;
                Ok(Json(MessageResponse {
                    session,
                    accepted: true,
                    answer: Some(answer),
                }))
            } else {
                handle
                    .submit(request.text, SubmitKind::Prompt)
                    .await
                    .map_err(api_error)?;
                Ok(Json(MessageResponse {
                    session,
                    accepted: true,
                    answer: None,
                }))
            }
        }
        other => Err(bad_request(format!(
            "mode must be prompt or steer; got {other:?}"
        ))),
    }
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<MessageResponse>> {
    authorize(&state, &headers)?;
    let handle = state
        .sessions
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown session: {id}")))?;
    let cancelled = handle.cancel().await;
    Ok(Json(MessageResponse {
        session: id,
        accepted: cancelled,
        answer: None,
    }))
}

async fn resolve_session(state: &AppState, id: Option<&str>) -> ApiResult<AgentHandle> {
    if let Some(id) = id {
        if let Some(handle) = state.sessions.lock().await.get(id).cloned() {
            return Ok(handle);
        }
        if Session::persisted_spec(&state.config.cwd, id)
            .await
            .map_err(api_error)?
            .is_none()
        {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("unknown session: {id}"),
            ));
        }
    }
    let initial = state.config.manager.current().await;
    let spec = execute::simple_spec(
        &initial.provider,
        &initial.model,
        initial.thinking,
        &state.config.cwd,
        true,
    );
    let handle = state.host.open_root(id, spec).await.map_err(api_error)?;
    state
        .sessions
        .lock()
        .await
        .insert(handle.session_id().to_string(), handle.clone());
    Ok(handle)
}
