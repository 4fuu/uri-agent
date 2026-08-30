use crate::acp::stdio::{AcpStdio, is_clean_eof};
use crate::agent::{AgentHandle, AgentHost, AgentOpenOptions, AgentPrompt, AgentSpec};
use crate::builtins::{
    SessionMcpProfile, SessionMcpServer, SessionMcpTransport, session_profile_owner,
    session_profile_record,
};
use crate::config::{Cli, Config, ConfigManager};
use crate::protocol::{ProtocolImage, ProtocolImageMediaType};
use crate::runtime::TurnOutcome;
use crate::session::{EventKind, Session, SessionEvent, SessionUpdate as NativeSessionUpdate};
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, Cost, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities, McpServer,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionInfo, SessionListCapabilities, SessionNotification, SessionResumeCapabilities,
    SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use agent_client_protocol::{
    Agent as AcpAgent, Client as AcpClient, ConnectTo, ConnectionTo, Dispatch, Error as AcpError,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

/// Serve stable ACP v1 over stdin/stdout.
pub async fn serve(cli: Cli) -> Result<()> {
    serve_on(cli, AcpStdio::stdio()).await
}

async fn serve_on(cli: Cli, transport: impl ConnectTo<AcpAgent> + 'static) -> Result<()> {
    let state = Arc::new(AcpV1State::new(cli));

    let initialize = state.clone();
    let new_session = state.clone();
    let load_session = state.clone();
    let resume_session = state.clone();
    let list_sessions = state.clone();
    let close_session = state.clone();
    let prompt = state.clone();
    let cancel = state.clone();
    let result = AcpAgent
        .builder()
        .name("uri-agent-acpv1")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                responder.respond(initialize.initialize(request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _connection| {
                respond(responder, new_session.new_session(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, connection| {
                respond(
                    responder,
                    load_session.load_session(request, &connection).await,
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, _connection| {
                respond(responder, resume_session.resume_session(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, _connection| {
                respond(responder, list_sessions.list_sessions(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest, responder, _connection| {
                respond(responder, close_session.close_session(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let session = match prompt.begin_prompt(&request.session_id.to_string()).await {
                    Ok(session) => session,
                    Err(error) => return responder.respond_with_error(error),
                };
                let task_connection = connection.clone();
                let task_session = session.clone();
                let spawn = connection.spawn({
                    let prompt = prompt.clone();
                    async move {
                        let result = prompt
                            .prompt(request, &task_connection, &task_session)
                            .await;
                        prompt.finish_prompt(&task_session).await;
                        respond(responder, result)
                    }
                });
                if let Err(error) = spawn {
                    prompt.finish_prompt(&session).await;
                    return Err(error);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                cancel.cancel(notification).await;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, connection: ConnectionTo<AcpClient>| {
                message.respond_with_error(AcpError::method_not_found(), connection)
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(transport)
        .await;

    state.shutdown().await;
    match result {
        Ok(()) => Ok(()),
        Err(error) if is_clean_eof(&error) => Ok(()),
        Err(error) => Err(anyhow!(error)),
    }
}

fn respond<T: agent_client_protocol::JsonRpcResponse>(
    responder: agent_client_protocol::Responder<T>,
    result: agent_client_protocol::Result<T>,
) -> agent_client_protocol::Result<()> {
    match result {
        Ok(response) => responder.respond(response),
        Err(error) => responder.respond_with_error(error),
    }
}

struct AcpProject {
    cwd: PathBuf,
    manager: Arc<ConfigManager>,
    host: AgentHost,
}

struct AcpSession {
    agent: AgentHandle,
    prompt: Mutex<AcpPromptState>,
}

#[derive(Default)]
struct AcpPromptState {
    active: bool,
    cancellation_requested: bool,
}

impl AcpPromptState {
    fn begin(&mut self) -> agent_client_protocol::Result<()> {
        if self.active {
            return Err(invalid_params(
                "a prompt is already active for this session",
            ));
        }
        self.active = true;
        self.cancellation_requested = false;
        Ok(())
    }

    fn request_cancellation(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.cancellation_requested = true;
        true
    }

    fn finish(&mut self) {
        self.active = false;
        self.cancellation_requested = false;
    }
}

struct AcpV1State {
    cli: Cli,
    project: Mutex<Option<Arc<AcpProject>>>,
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
}

impl AcpV1State {
    fn new(cli: Cli) -> Self {
        Self {
            cli,
            project: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn initialize(&self, request: InitializeRequest) -> InitializeResponse {
        let sessions = SessionCapabilities::new()
            .list(SessionListCapabilities::new())
            .resume(SessionResumeCapabilities::new())
            .close(SessionCloseCapabilities::new());
        let capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(PromptCapabilities::new().image(true))
            .mcp_capabilities(McpCapabilities::new().http(true))
            .session_capabilities(sessions);
        InitializeResponse::new(request.protocol_version)
            .agent_capabilities(capabilities)
            .agent_info(Implementation::new("uri-agent", env!("CARGO_PKG_VERSION")))
    }

    async fn project(&self, cwd: &Path) -> Result<Arc<AcpProject>> {
        if !cwd.is_absolute() {
            bail!("ACP session cwd must be an absolute path");
        }
        let cwd = tokio::fs::canonicalize(cwd)
            .await
            .with_context(|| format!("cannot resolve ACP session cwd {}", cwd.display()))?;
        let mut project = self.project.lock().await;
        if let Some(project) = project.as_ref() {
            if project.cwd != cwd {
                bail!(
                    "this ACP process is already bound to project {}",
                    project.cwd.display()
                );
            }
            return Ok(project.clone());
        }

        let mut cli = self.cli.clone();
        cli.acpv1 = false;
        cli.cwd = Some(cwd.clone());
        cli.continue_session = false;
        cli.session = None;
        cli.background = false;
        let config = Config::load(cli).await?;
        let manager = config.manager.clone();
        let host = AgentHost::new(
            config.manager,
            config.environment,
            config.catalog,
            config.cwd,
        )
        .await?;
        let initialized = Arc::new(AcpProject { cwd, manager, host });
        *project = Some(initialized.clone());
        Ok(initialized)
    }

    async fn new_session(
        &self,
        request: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        reject_additional_directories(&request.additional_directories)?;
        let project = self.project(&request.cwd).await.map_err(invalid_params)?;
        let profile = mcp_profile(request.mcp_servers)?;
        let (owner, payload) = session_profile_record(profile).map_err(internal_error)?;
        let mut options = AgentOpenOptions::default();
        options.private_records.insert(owner, payload);
        let initial = project.manager.current().await;
        let agent = project
            .host
            .open_root_with_options(
                None,
                AgentSpec::root(
                    &initial.provider,
                    &initial.model,
                    initial.thinking,
                    &project.cwd,
                ),
                options,
            )
            .await
            .map_err(invalid_params)?;
        if let Err(error) = prepare_and_persist(&agent).await {
            agent.close().await;
            return Err(invalid_params(error));
        }
        let session_id = agent.session_id().to_string();
        self.insert_session(agent).await?;
        Ok(NewSessionResponse::new(session_id))
    }

    async fn load_session(
        &self,
        request: LoadSessionRequest,
        connection: &ConnectionTo<AcpClient>,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        reject_additional_directories(&request.additional_directories)?;
        let session_id = request.session_id.to_string();
        let agent = self
            .open_existing(&request.cwd, &session_id, request.mcp_servers)
            .await?;
        let events = agent
            .services()
            .runtime
            .session()
            .snapshot()
            .await
            .map_err(internal_error)?;
        let mut projector = EventProjector::replay(
            session_id,
            connection.clone(),
            agent.services().context_window,
        );
        for event in events {
            projector.persisted(&event).map_err(internal_error)?;
        }
        Ok(LoadSessionResponse::new())
    }

    async fn resume_session(
        &self,
        request: ResumeSessionRequest,
    ) -> agent_client_protocol::Result<ResumeSessionResponse> {
        reject_additional_directories(&request.additional_directories)?;
        let session_id = request.session_id.to_string();
        self.open_existing(&request.cwd, &session_id, request.mcp_servers)
            .await?;
        Ok(ResumeSessionResponse::new())
    }

    async fn open_existing(
        &self,
        cwd: &Path,
        session_id: &str,
        mcp_servers: Vec<McpServer>,
    ) -> agent_client_protocol::Result<AgentHandle> {
        if self.sessions.lock().await.contains_key(session_id) {
            return Err(invalid_params("ACP session is already active"));
        }
        let project = self.project(cwd).await.map_err(invalid_params)?;
        let spec = Session::persisted_spec(&project.cwd, session_id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| invalid_params(format!("unknown session {session_id}")))?;
        let requested_profile = mcp_profile(mcp_servers)?;
        let stored_profile =
            Session::persisted_private_record(&project.cwd, session_id, session_profile_owner())
                .await
                .map_err(internal_error)?;
        let mut options = AgentOpenOptions::default();
        match stored_profile {
            Some(stored) => {
                let stored: SessionMcpProfile = serde_json::from_value(stored)
                    .map_err(|_| internal_error("invalid stored MCP session profile"))?;
                if stored.servers.keys().ne(requested_profile.servers.keys()) {
                    return Err(invalid_params(
                        "MCP server names must match the session's frozen protocol set",
                    ));
                }
                let (owner, payload) =
                    session_profile_record(requested_profile).map_err(internal_error)?;
                options.private_records.insert(owner, payload);
            }
            None if !requested_profile.servers.is_empty() => {
                return Err(invalid_params(
                    "cannot replace configured MCP protocols on an existing native session",
                ));
            }
            None => {}
        }
        let agent = project
            .host
            .open_root_with_options(Some(session_id), spec, options)
            .await
            .map_err(invalid_params)?;
        if let Err(error) = agent.services().runtime.prepare_context().await {
            agent.close().await;
            return Err(invalid_params(error));
        }
        self.insert_session(agent.clone()).await?;
        Ok(agent)
    }

    async fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> agent_client_protocol::Result<ListSessionsResponse> {
        if request.cursor.is_some() {
            return Err(invalid_params("session/list cursor is not supported"));
        }
        let project = match request.cwd {
            Some(cwd) => Some(self.project(&cwd).await.map_err(invalid_params)?),
            None => self.project.lock().await.clone(),
        };
        let Some(project) = project else {
            return Ok(ListSessionsResponse::new(Vec::new()));
        };
        let sessions = Session::list_for(&project.cwd)
            .await
            .map_err(internal_error)?
            .into_iter()
            .map(|session| {
                SessionInfo::new(session.id, project.cwd.clone())
                    .title(session.preview)
                    .updated_at(session.updated_at.to_rfc3339())
            })
            .collect();
        Ok(ListSessionsResponse::new(sessions))
    }

    async fn close_session(
        &self,
        request: CloseSessionRequest,
    ) -> agent_client_protocol::Result<CloseSessionResponse> {
        let session_id = request.session_id.to_string();
        let session = self
            .sessions
            .lock()
            .await
            .remove(&session_id)
            .ok_or_else(|| invalid_params(format!("unknown active session {session_id}")))?;
        session.agent.cancel().await;
        session.agent.close().await;
        Ok(CloseSessionResponse::new())
    }

    async fn prompt(
        &self,
        request: PromptRequest,
        connection: &ConnectionTo<AcpClient>,
        session: &Arc<AcpSession>,
    ) -> agent_client_protocol::Result<PromptResponse> {
        let session_id = request.session_id.to_string();
        let prompt = convert_prompt(request.prompt)?;
        let runtime = session.agent.services().runtime.clone();
        let mut updates = runtime.session().subscribe();
        let mut completions = runtime.subscribe_turn_completions();
        let events = runtime.session().snapshot().await.map_err(internal_error)?;
        let mut projector = EventProjector::live(
            session_id,
            connection.clone(),
            session.agent.services().context_window,
            cumulative_cost(&events),
        );
        let submission_id = session
            .agent
            .submit_prompt_exclusive(prompt)
            .await
            .map_err(prompt_error)?;
        if session.prompt.lock().await.cancellation_requested {
            session.agent.cancel().await;
        }

        let completion = loop {
            tokio::select! {
                update = updates.recv() => match update {
                    Ok(update) => projector.update(update).map_err(internal_error)?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        return Err(internal_error("ACP session update stream lagged"));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(internal_error("native session update stream closed"));
                    }
                },
                completion = completions.recv() => match completion {
                    Ok(completion) if completion.submission_id == submission_id => break completion,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        return Err(internal_error("turn completion stream lagged"));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(internal_error("turn completion stream closed"));
                    }
                },
            }
        };
        drain_updates(&mut updates, &mut projector)?;
        match completion.outcome {
            TurnOutcome::Completed => Ok(PromptResponse::new(StopReason::EndTurn)),
            TurnOutcome::Cancelled => Ok(PromptResponse::new(StopReason::Cancelled)),
            TurnOutcome::Failed(error) => Err(prompt_error(error)),
        }
    }

    async fn cancel(&self, notification: CancelNotification) {
        let session_id = notification.session_id.to_string();
        if let Some(session) = self.sessions.lock().await.get(&session_id).cloned()
            && Self::request_prompt_cancellation(&session).await
        {
            session.agent.cancel().await;
        }
    }

    async fn begin_prompt(
        &self,
        session_id: &str,
    ) -> agent_client_protocol::Result<Arc<AcpSession>> {
        let session = self.session(session_id).await?;
        let mut prompt = session.prompt.lock().await;
        prompt.begin()?;
        drop(prompt);
        Ok(session)
    }

    async fn request_prompt_cancellation(session: &AcpSession) -> bool {
        session.prompt.lock().await.request_cancellation()
    }

    async fn finish_prompt(&self, session: &AcpSession) {
        session.prompt.lock().await.finish();
    }

    async fn session(&self, session_id: &str) -> agent_client_protocol::Result<Arc<AcpSession>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| invalid_params(format!("unknown active session {session_id}")))
    }

    async fn insert_session(&self, agent: AgentHandle) -> agent_client_protocol::Result<()> {
        let session_id = agent.session_id().to_string();
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session_id) {
            return Err(invalid_params(format!(
                "ACP session {session_id} is already active"
            )));
        }
        sessions.insert(
            session_id,
            Arc::new(AcpSession {
                agent,
                prompt: Mutex::new(AcpPromptState::default()),
            }),
        );
        Ok(())
    }

    async fn shutdown(&self) {
        let sessions = self
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, session)| session)
            .collect::<Vec<_>>();
        for session in sessions {
            session.agent.close().await;
        }
    }
}

async fn prepare_and_persist(agent: &AgentHandle) -> Result<()> {
    let runtime = &agent.services().runtime;
    runtime.prepare_context().await?;
    runtime.session().persist().await
}

fn reject_additional_directories(directories: &[PathBuf]) -> agent_client_protocol::Result<()> {
    if directories.is_empty() {
        Ok(())
    } else {
        Err(invalid_params(
            "additionalDirectories are not supported by uri-agent --acpv1",
        ))
    }
}

fn mcp_profile(servers: Vec<McpServer>) -> agent_client_protocol::Result<SessionMcpProfile> {
    let mut mapped = BTreeMap::new();
    for server in servers {
        let (name, transport) = match server {
            McpServer::Stdio(server) => {
                let command = server
                    .command
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid_params("MCP stdio command must be valid Unicode"))?;
                let environment = unique_pairs(
                    server
                        .env
                        .into_iter()
                        .map(|entry| (entry.name, entry.value)),
                    "MCP environment variable",
                )?;
                (
                    server.name,
                    SessionMcpTransport::Stdio {
                        command,
                        args: server.args,
                        environment,
                    },
                )
            }
            McpServer::Http(server) => {
                let headers = unique_pairs(
                    server
                        .headers
                        .into_iter()
                        .map(|header| (header.name, header.value)),
                    "MCP HTTP header",
                )?;
                (
                    server.name,
                    SessionMcpTransport::StreamableHttp {
                        url: server.url,
                        headers,
                    },
                )
            }
            McpServer::Sse(_) => {
                return Err(invalid_params("MCP SSE transport is not supported"));
            }
            _ => return Err(invalid_params("unsupported MCP transport")),
        };
        if mapped
            .insert(name.clone(), SessionMcpServer { transport })
            .is_some()
        {
            return Err(invalid_params(format!(
                "MCP server name {name:?} is duplicated"
            )));
        }
    }
    Ok(SessionMcpProfile::new(mapped))
}

fn unique_pairs(
    pairs: impl IntoIterator<Item = (String, String)>,
    label: &str,
) -> agent_client_protocol::Result<BTreeMap<String, String>> {
    let mut mapped = BTreeMap::new();
    for (name, value) in pairs {
        if mapped.insert(name.clone(), value).is_some() {
            return Err(invalid_params(format!("{label} {name:?} is duplicated")));
        }
    }
    Ok(mapped)
}

fn convert_prompt(blocks: Vec<ContentBlock>) -> agent_client_protocol::Result<AgentPrompt> {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(content) => text.push(content.text),
            ContentBlock::ResourceLink(resource) => {
                text.push(format!("[Resource {}]({})", resource.name, resource.uri))
            }
            ContentBlock::Image(image) => {
                let media_type = match image.mime_type.to_ascii_lowercase().as_str() {
                    "image/jpeg" | "image/jpg" => ProtocolImageMediaType::Jpeg,
                    "image/png" => ProtocolImageMediaType::Png,
                    "image/gif" => ProtocolImageMediaType::Gif,
                    "image/webp" => ProtocolImageMediaType::Webp,
                    _ => return Err(invalid_params("unsupported ACP image MIME type")),
                };
                let bytes = BASE64
                    .decode(image.data)
                    .map_err(|_| invalid_params("ACP image data is not valid base64"))?;
                if ProtocolImageMediaType::detect(&bytes) != Some(media_type) {
                    return Err(invalid_params(
                        "ACP image data does not match its declared MIME type",
                    ));
                }
                images.push(ProtocolImage::new(bytes, media_type));
            }
            ContentBlock::Audio(_) => {
                return Err(invalid_params("ACP audio prompts are not supported"));
            }
            ContentBlock::Resource(_) => {
                return Err(invalid_params("embedded ACP resources are not supported"));
            }
            _ => return Err(invalid_params("unsupported ACP prompt content")),
        }
    }
    let mut text = text.join("\n\n");
    if text.trim().is_empty() && !images.is_empty() {
        text = "Image attachment".to_string();
    }
    if text.trim().is_empty() {
        return Err(invalid_params("ACP prompt is empty"));
    }
    Ok(AgentPrompt { text, images })
}

fn cumulative_cost(events: &[SessionEvent]) -> f64 {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Usage { cost, .. } => Some(*cost),
            _ => None,
        })
        .sum()
}

fn drain_updates(
    updates: &mut broadcast::Receiver<NativeSessionUpdate>,
    projector: &mut EventProjector,
) -> agent_client_protocol::Result<()> {
    loop {
        match updates.try_recv() {
            Ok(update) => projector.update(update).map_err(internal_error)?,
            Err(broadcast::error::TryRecvError::Empty) => return Ok(()),
            Err(broadcast::error::TryRecvError::Closed) => return Ok(()),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                return Err(internal_error("ACP session update stream lagged"));
            }
        }
    }
}

enum ProjectionMode {
    Live,
    Replay,
}

struct EventProjector {
    session_id: String,
    connection: ConnectionTo<AcpClient>,
    mode: ProjectionMode,
    context_window: u64,
    cost: f64,
}

impl EventProjector {
    fn live(
        session_id: String,
        connection: ConnectionTo<AcpClient>,
        context_window: usize,
        cost: f64,
    ) -> Self {
        Self::new(
            session_id,
            connection,
            ProjectionMode::Live,
            context_window,
            cost,
        )
    }

    fn replay(
        session_id: String,
        connection: ConnectionTo<AcpClient>,
        context_window: usize,
    ) -> Self {
        Self::new(
            session_id,
            connection,
            ProjectionMode::Replay,
            context_window,
            0.0,
        )
    }

    fn new(
        session_id: String,
        connection: ConnectionTo<AcpClient>,
        mode: ProjectionMode,
        context_window: usize,
        cost: f64,
    ) -> Self {
        Self {
            session_id,
            connection,
            mode,
            context_window: context_window as u64,
            cost,
        }
    }

    fn update(&mut self, update: NativeSessionUpdate) -> Result<()> {
        match update {
            // Native deltas are provisional and may be discarded during a model
            // retry. ACP chunks are append-only, so publish only committed events.
            NativeSessionUpdate::Transient(_) => Ok(()),
            NativeSessionUpdate::Persisted(event) => self.persisted(&event),
        }
    }

    fn persisted(&mut self, event: &SessionEvent) -> Result<()> {
        match &event.kind {
            EventKind::User { text } if matches!(self.mode, ProjectionMode::Replay) => {
                self.send(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(text)),
                )))
            }
            EventKind::AssistantText { text } => self.agent_text(text.clone()),
            EventKind::AssistantReasoning { text } => self.thought(text.clone()),
            EventKind::ToolCall {
                call_id,
                name,
                arguments,
            } => self.send(SessionUpdate::ToolCall(
                ToolCall::new(call_id.clone(), tool_title(name, arguments))
                    .kind(tool_kind(name))
                    .status(ToolCallStatus::InProgress)
                    .raw_input(arguments.clone()),
            )),
            EventKind::ToolResult {
                call_id,
                output,
                failed,
                ..
            } => {
                let status = if *failed {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                };
                let content = vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
                    output,
                )))];
                self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    call_id.clone(),
                    ToolCallUpdateFields::new()
                        .status(status)
                        .content(content)
                        .raw_output(serde_json::json!(output)),
                )))
            }
            EventKind::Usage { cost, total, .. } => {
                self.cost += cost;
                self.send(SessionUpdate::UsageUpdate(
                    UsageUpdate::new(*total, self.context_window).cost(Cost::new(self.cost, "USD")),
                ))
            }
            _ => Ok(()),
        }
    }

    fn agent_text(&self, text: String) -> Result<()> {
        self.send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )))
    }

    fn thought(&self, text: String) -> Result<()> {
        self.send(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )))
    }

    fn send(&self, update: SessionUpdate) -> Result<()> {
        self.connection
            .send_notification(SessionNotification::new(self.session_id.clone(), update))
            .map_err(|error| anyhow!(error))
    }
}

fn tool_title(name: &str, arguments: &serde_json::Value) -> String {
    if arguments.is_null() || arguments.as_object().is_some_and(serde_json::Map::is_empty) {
        name.to_string()
    } else {
        format!("{name} {arguments}")
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" => ToolKind::Read,
        "exec" => ToolKind::Execute,
        "replace" | "apply_patch" => ToolKind::Edit,
        _ => ToolKind::Other,
    }
}

fn invalid_params(error: impl std::fmt::Display) -> AcpError {
    AcpError::invalid_params().data(error.to_string())
}

fn internal_error(error: impl std::fmt::Display) -> AcpError {
    AcpError::internal_error().data(error.to_string())
}

fn prompt_error(error: impl std::fmt::Display) -> AcpError {
    let error = error.to_string();
    if error.contains("no credential configured") {
        return internal_error(
            "no model credential is configured; configure authentication before starting uri-agent --acpv1",
        );
    }
    internal_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Channel;
    use agent_client_protocol::schema::{
        ProtocolVersion,
        v1::{EnvVariable, McpServerStdio, ResourceLink},
    };
    use clap::Parser;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn line_transport_dispatches_and_stops_on_input_eof() {
        let cli = Cli::try_parse_from(["uri-agent", "--acpv1", "--offline"]).unwrap();
        let (mut client_input, agent_input) = tokio::io::duplex(4096);
        let (agent_output, client_output) = tokio::io::duplex(4096);
        let agent = tokio::spawn(serve_on(cli, AcpStdio::new(agent_output, agent_input)));
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "stdio-test", "version": "1"},
            },
        });
        client_input
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut output = BufReader::new(client_output);
        let mut line = String::new();
        output.read_line(&mut line).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], 1);

        drop(client_input);
        tokio::time::timeout(std::time::Duration::from_secs(1), agent)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn line_transport_parse_errors_do_not_echo_input() {
        let cli = Cli::try_parse_from(["uri-agent", "--acpv1", "--offline"]).unwrap();
        let (mut client_input, agent_input) = tokio::io::duplex(4096);
        let (agent_output, client_output) = tokio::io::duplex(4096);
        let agent = tokio::spawn(serve_on(cli, AcpStdio::new(agent_output, agent_input)));
        client_input
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"token\":\"literal-secret\"}\n",
            )
            .await
            .unwrap();
        let mut output = BufReader::new(client_output);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            output.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["error"]["code"], -32700);
        assert!(!line.contains("literal-secret"));

        drop(client_input);
        tokio::time::timeout(std::time::Duration::from_secs(1), agent)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn transport_dispatches_initialize_and_projectless_list() {
        let cli = Cli::try_parse_from(["uri-agent", "--acpv1", "--offline"]).unwrap();
        let (client_transport, agent_transport) = Channel::duplex();
        let agent = tokio::spawn(serve_on(cli, agent_transport));

        AcpClient
            .builder()
            .connect_with(client_transport, async |connection| {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
                let listed = connection
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert!(listed.sessions.is_empty());
                assert!(listed.next_cursor.is_none());
                Ok(())
            })
            .await
            .unwrap();
        agent.abort();
        assert!(agent.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn prompt_state_retains_early_cancellation_until_submission() {
        let mut prompt = AcpPromptState::default();
        assert!(!prompt.request_cancellation());
        prompt.begin().unwrap();
        assert!(prompt.begin().is_err());
        assert!(prompt.request_cancellation());
        assert!(prompt.cancellation_requested);
        prompt.finish();
        assert!(!prompt.active);
        assert!(!prompt.cancellation_requested);
    }

    #[test]
    fn prompt_mapping_supports_baseline_links_and_typed_images() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01";
        let prompt = convert_prompt(vec![
            ContentBlock::Text(TextContent::new("inspect")),
            ContentBlock::ResourceLink(ResourceLink::new("source", "file:///tmp/source.rs")),
            ContentBlock::Image(agent_client_protocol::schema::v1::ImageContent::new(
                BASE64.encode(png),
                "image/png",
            )),
        ])
        .unwrap();
        assert_eq!(
            prompt.text,
            "inspect\n\n[Resource source](file:///tmp/source.rs)"
        );
        assert_eq!(prompt.images.len(), 1);
    }

    #[test]
    fn mcp_mapping_keeps_literal_secrets_private_and_rejects_duplicates() {
        let profile = mcp_profile(vec![McpServer::Stdio(
            McpServerStdio::new("local", "/bin/server")
                .env(vec![EnvVariable::new("TOKEN", "literal-secret")]),
        )])
        .unwrap();
        let serialized = serde_json::to_string(&profile).unwrap();
        assert!(serialized.contains("literal-secret"));
        assert!(
            mcp_profile(vec![
                McpServer::Stdio(McpServerStdio::new("same", "/bin/a")),
                McpServer::Stdio(McpServerStdio::new("same", "/bin/b")),
            ])
            .is_err()
        );
        let colliding = mcp_profile(vec![
            McpServer::Stdio(McpServerStdio::new("same name", "/bin/a")),
            McpServer::Stdio(McpServerStdio::new("same-name", "/bin/b")),
        ])
        .unwrap();
        assert!(session_profile_record(colliding).is_err());
    }
}
