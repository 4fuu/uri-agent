use crate::acp::stdio::{AcpStdio, is_clean_eof};
use crate::agent::{AgentHandle, AgentHost, AgentOpenOptions, AgentPrompt, AgentSpec};
use crate::builtins::{
    SessionMcpProfile, SessionMcpServer, SessionMcpTransport, session_profile_owner,
    session_profile_record,
};
use crate::catalog::{CatalogModel, ModelCatalog, ThinkingLevel};
use crate::config::{Cli, Config, ConfigManager};
use crate::model::clamp_thinking_level;
use crate::protocol::{ProtocolImage, ProtocolImageMediaType};
use crate::runtime::TurnOutcome;
use crate::session::{EventKind, Session, SessionEvent, SessionUpdate as NativeSessionUpdate};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    ConfigOptionUpdate, ContentBlock, ContentChunk, Cost, ImageContent, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, McpCapabilities, McpServer, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, ResumeSessionRequest,
    ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectGroup, SessionConfigSelectOption, SessionInfo,
    SessionListCapabilities, SessionNotification, SessionResumeCapabilities, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, TextContent,
    ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    UsageUpdate,
};
use agent_client_protocol::{
    Agent as AcpAgent, Client as AcpClient, ConnectTo, ConnectionTo, Dispatch, Error as AcpError,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use http::HeaderName;
use rig::message::{DocumentSourceKind, ImageMediaType, Message, UserContent};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Serve stable ACP v1 over stdin/stdout.
pub async fn serve(cli: Cli) -> Result<()> {
    serve_on(cli, AcpStdio::stdio()).await
}

async fn serve_on(cli: Cli, transport: impl ConnectTo<AcpAgent> + 'static) -> Result<()> {
    let state = Arc::new(AcpV1State::new(cli));
    serve_state_on(state, transport).await
}

async fn serve_state_on(
    state: Arc<AcpV1State>,
    transport: impl ConnectTo<AcpAgent> + 'static,
) -> Result<()> {
    let initialize = state.clone();
    let new_session = state.clone();
    let load_session = state.clone();
    let resume_session = state.clone();
    let list_sessions = state.clone();
    let close_session = state.clone();
    let set_config_option = state.clone();
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
            async move |request: ResumeSessionRequest, responder, connection| {
                respond(
                    responder,
                    resume_session.resume_session(request, &connection).await,
                )
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
            async move |request: SetSessionConfigOptionRequest, responder, _connection| {
                respond(
                    responder,
                    set_config_option.set_config_option(request).await,
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let session = match prompt.begin_prompt(&request.session_id.to_string()).await {
                    Ok(session) => session,
                    Err(error) => return responder.respond_with_error(error),
                };
                let task_session = session.clone();
                let task_connection = connection.clone();
                let spawn = connection.spawn({
                    let prompt = prompt.clone();
                    async move {
                        let result = prompt
                            .prompt(request, &task_session, &task_connection)
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
    catalog: Arc<ModelCatalog>,
    host: AgentHost,
}

struct AcpSession {
    id: String,
    cwd: PathBuf,
    created_at: DateTime<Utc>,
    state: Mutex<AcpSessionState>,
    prompt: Mutex<AcpPromptState>,
}

enum AcpSessionState {
    Pending(Box<AcpPendingSession>),
    Active(Arc<AcpActiveSession>),
    Closed,
}

struct AcpPendingSession {
    project: Arc<AcpProject>,
    spec: AgentSpec,
    options: AgentOpenOptions,
}

struct AcpActiveSession {
    agent: AgentHandle,
    spec: AgentSpec,
    projection: ProjectionHandle,
}

#[derive(Default)]
struct AcpPromptState {
    active: bool,
    cancellation_requested: bool,
    submission_id: Option<u64>,
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
        self.submission_id = None;
        Ok(())
    }

    fn request_cancellation(&mut self) -> Option<u64> {
        if !self.active {
            return None;
        }
        self.cancellation_requested = true;
        self.submission_id
    }

    fn submitted(&mut self, submission_id: u64) -> bool {
        if !self.active {
            return false;
        }
        self.submission_id = Some(submission_id);
        self.cancellation_requested
    }

    fn finish(&mut self) {
        self.active = false;
        self.cancellation_requested = false;
        self.submission_id = None;
    }
}

struct AcpV1State {
    cli: Cli,
    projects: Mutex<HashMap<PathBuf, Arc<AcpProject>>>,
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
}

impl AcpV1State {
    fn new(cli: Cli) -> Self {
        Self {
            cli,
            projects: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn initialize(&self, _request: InitializeRequest) -> InitializeResponse {
        let sessions = SessionCapabilities::new()
            .list(SessionListCapabilities::new())
            .resume(SessionResumeCapabilities::new())
            .close(SessionCloseCapabilities::new());
        let capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(PromptCapabilities::new().image(true))
            .mcp_capabilities(McpCapabilities::new().http(true))
            .session_capabilities(sessions);
        InitializeResponse::new(ProtocolVersion::V1)
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
        let mut projects = self.projects.lock().await;
        if let Some(project) = projects.get(&cwd) {
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
        let catalog = config.catalog.clone();
        let host = AgentHost::new(
            config.manager,
            config.environment,
            config.catalog,
            config.cwd,
        )
        .await?;
        let initialized = Arc::new(AcpProject {
            cwd,
            manager,
            catalog,
            host,
        });
        projects.insert(initialized.cwd.clone(), initialized.clone());
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
        let spec = AgentSpec::root(
            &initial.provider,
            &initial.model,
            initial.thinking,
            &project.cwd,
        );
        let config_options = pending_config_options(&project, &spec).await;
        let session_id = Session::new_id();
        let session = Arc::new(AcpSession {
            id: session_id.clone(),
            cwd: project.cwd.clone(),
            created_at: Utc::now(),
            state: Mutex::new(AcpSessionState::Pending(Box::new(AcpPendingSession {
                project,
                spec,
                options,
            }))),
            prompt: Mutex::new(AcpPromptState::default()),
        });
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session_id) {
            return Err(internal_error("generated a duplicate ACP session ID"));
        }
        sessions.insert(session_id.clone(), session);
        drop(sessions);
        Ok(NewSessionResponse::new(session_id).config_options(config_options))
    }

    async fn load_session(
        &self,
        request: LoadSessionRequest,
        connection: &ConnectionTo<AcpClient>,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        reject_additional_directories(&request.additional_directories)?;
        let session_id = request.session_id.to_string();
        let active = self
            .open_existing(
                &request.cwd,
                &session_id,
                request.mcp_servers,
                connection,
                true,
            )
            .await?;
        Ok(LoadSessionResponse::new().config_options(frozen_config_options(&active.spec)))
    }

    async fn resume_session(
        &self,
        request: ResumeSessionRequest,
        connection: &ConnectionTo<AcpClient>,
    ) -> agent_client_protocol::Result<ResumeSessionResponse> {
        reject_additional_directories(&request.additional_directories)?;
        let session_id = request.session_id.to_string();
        let active = self
            .open_existing(
                &request.cwd,
                &session_id,
                request.mcp_servers,
                connection,
                false,
            )
            .await?;
        Ok(ResumeSessionResponse::new().config_options(frozen_config_options(&active.spec)))
    }

    async fn open_existing(
        &self,
        cwd: &Path,
        session_id: &str,
        mcp_servers: Vec<McpServer>,
        connection: &ConnectionTo<AcpClient>,
        replay: bool,
    ) -> agent_client_protocol::Result<Arc<AcpActiveSession>> {
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
            .open_root_with_deferred_resume(Some(session_id), spec.clone(), options)
            .await
            .map_err(invalid_params)?;
        if let Err(error) = agent.services().runtime.prepare_context().await {
            agent.close().await;
            return Err(invalid_params(error));
        }
        let active = self
            .insert_active_session(agent, spec, connection, replay)
            .await?;
        if let Err(error) = active.agent.services().runtime.resume_pending().await {
            self.sessions.lock().await.remove(session_id);
            close_active_session(&active, false).await;
            return Err(invalid_params(error));
        }
        Ok(active)
    }

    async fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> agent_client_protocol::Result<ListSessionsResponse> {
        if request.cursor.is_some() {
            return Err(invalid_params("session/list cursor is not supported"));
        }
        let filtered_by_project = request.cwd.is_some();
        let projects = match request.cwd {
            Some(cwd) => vec![self.project(&cwd).await.map_err(invalid_params)?],
            None => self.projects.lock().await.values().cloned().collect(),
        };
        if projects.is_empty() {
            return Ok(ListSessionsResponse::new(Vec::new()));
        }
        let project_filter = filtered_by_project.then(|| projects[0].cwd.as_path());
        let active_sessions = self
            .sessions
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut pending = Vec::new();
        for session in active_sessions {
            if project_filter.is_none_or(|cwd| cwd == session.cwd)
                && matches!(*session.state.lock().await, AcpSessionState::Pending(_))
            {
                pending.push((
                    session.created_at,
                    SessionInfo::new(session.id.clone(), session.cwd.clone())
                        .title("empty session")
                        .updated_at(session.created_at.to_rfc3339()),
                ));
            }
        }
        let mut sessions = Vec::new();
        for project in projects {
            sessions.extend(
                Session::list_for(&project.cwd)
                    .await
                    .map_err(internal_error)?
                    .into_iter()
                    .map(|session| {
                        (
                            session.updated_at,
                            SessionInfo::new(session.id, project.cwd.clone())
                                .title(session.preview)
                                .updated_at(session.updated_at.to_rfc3339()),
                        )
                    }),
            );
        }
        let mut listed = sessions
            .iter()
            .map(|(_, session)| session.session_id.to_string())
            .collect::<HashSet<_>>();
        sessions.extend(
            pending
                .into_iter()
                .filter(|(_, session)| listed.insert(session.session_id.to_string())),
        );
        sessions.sort_by(|(left_at, left), (right_at, right)| {
            right_at.cmp(left_at).then_with(|| {
                right
                    .session_id
                    .to_string()
                    .cmp(&left.session_id.to_string())
            })
        });
        Ok(ListSessionsResponse::new(
            sessions.into_iter().map(|(_, session)| session).collect(),
        ))
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
        close_acp_session(&session, true).await;
        Ok(CloseSessionResponse::new())
    }

    async fn set_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::Result<SetSessionConfigOptionResponse> {
        let session = self.session(&request.session_id.to_string()).await?;
        let prompt = session.prompt.lock().await;
        if prompt.active {
            return Err(invalid_params(
                "session configuration freezes when the first prompt starts",
            ));
        }
        let config_id = request.config_id.0.as_ref();
        let value = request.value.0.as_ref();
        let mut state = session.state.lock().await;
        let config_options = match &mut *state {
            AcpSessionState::Pending(pending) => {
                match config_id {
                    MODEL_CONFIG_ID => {
                        let (provider, model) = decode_model_value(value)?;
                        if provider != pending.spec.provider || model != pending.spec.model {
                            let selected = selectable_model(pending, &provider, &model)
                                .await
                                .ok_or_else(|| invalid_params("unknown ACP model option"))?;
                            let requested = pending
                                .project
                                .manager
                                .thinking_for_model(&provider, &model)
                                .await;
                            pending.spec.provider = provider;
                            pending.spec.model = model;
                            pending.spec.thinking = clamp_thinking_level(&selected, requested);
                        }
                    }
                    THINKING_CONFIG_ID => {
                        let requested = value.parse::<ThinkingLevel>().map_err(invalid_params)?;
                        let model = pending
                            .project
                            .catalog
                            .model(&pending.spec.provider, &pending.spec.model)
                            .await;
                        if requested != pending.spec.thinking
                            && !model
                                .as_ref()
                                .is_some_and(|model| model.supports_thinking_level(requested))
                        {
                            return Err(invalid_params(
                                "thinking level is not supported by the selected model",
                            ));
                        }
                        pending.spec.thinking = requested;
                    }
                    _ => return Err(invalid_params("unknown ACP session configuration option")),
                }
                pending_config_options(&pending.project, &pending.spec).await
            }
            AcpSessionState::Active(active) => {
                validate_frozen_config_value(&active.spec, config_id, value)?;
                frozen_config_options(&active.spec)
            }
            AcpSessionState::Closed => return Err(invalid_params("ACP session is closed")),
        };
        drop(state);
        drop(prompt);
        Ok(SetSessionConfigOptionResponse::new(config_options))
    }

    async fn prompt(
        &self,
        request: PromptRequest,
        session: &Arc<AcpSession>,
        connection: &ConnectionTo<AcpClient>,
    ) -> agent_client_protocol::Result<PromptResponse> {
        let prompt = convert_prompt(request.prompt)?;
        let (active, mut completions, submission_id, froze) =
            self.submit_prompt(session, prompt, connection).await?;
        if froze {
            connection
                .send_notification(SessionNotification::new(
                    session.id.clone(),
                    SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                        frozen_config_options(&active.spec),
                    )),
                ))
                .map_err(internal_error)?;
        }
        if session.prompt.lock().await.submitted(submission_id) {
            active.agent.cancel_submission(submission_id).await;
        }

        let completion = loop {
            match completions.recv().await {
                Ok(completion) if completion.submission_id == submission_id => break completion,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    return Err(internal_error("turn completion stream lagged"));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(internal_error("turn completion stream closed"));
                }
            }
        };
        let runtime = active.agent.services().runtime.clone();
        let through_sequence = runtime.session().head_sequence().await;
        active
            .projection
            .wait_through(through_sequence)
            .await
            .map_err(internal_error)?;
        match completion.outcome {
            TurnOutcome::Completed => Ok(PromptResponse::new(StopReason::EndTurn)),
            TurnOutcome::Cancelled => Ok(PromptResponse::new(StopReason::Cancelled)),
            TurnOutcome::Failed(error) => Err(prompt_error(error)),
        }
    }

    async fn submit_prompt(
        &self,
        session: &AcpSession,
        prompt: AgentPrompt,
        connection: &ConnectionTo<AcpClient>,
    ) -> agent_client_protocol::Result<(
        Arc<AcpActiveSession>,
        broadcast::Receiver<crate::runtime::TurnCompletion>,
        u64,
        bool,
    )> {
        let mut state = session.state.lock().await;
        match &*state {
            AcpSessionState::Active(active) => {
                let active = active.clone();
                let completions = active.agent.services().runtime.subscribe_turn_completions();
                let submission_id = active
                    .agent
                    .submit_prompt_exclusive(prompt)
                    .await
                    .map_err(prompt_error)?;
                Ok((active, completions, submission_id, false))
            }
            AcpSessionState::Pending(pending) => {
                let spec = pending.spec.clone();
                let agent = pending
                    .project
                    .host
                    .open_root_with_options(
                        Some(&session.id),
                        spec.clone(),
                        pending.options.clone(),
                    )
                    .await
                    .map_err(invalid_params)?;
                let projection =
                    match ProjectionHandle::start(&agent, connection.clone(), false).await {
                        Ok(projection) => projection,
                        Err(error) => {
                            agent.close().await;
                            return Err(internal_error(error));
                        }
                    };
                let active = Arc::new(AcpActiveSession {
                    agent,
                    spec,
                    projection,
                });
                let completions = active.agent.services().runtime.subscribe_turn_completions();
                let submission_id = match active.agent.submit_prompt_exclusive(prompt).await {
                    Ok(submission_id) => submission_id,
                    Err(error) => {
                        close_active_session(&active, false).await;
                        return Err(prompt_error(error));
                    }
                };
                *state = AcpSessionState::Active(active.clone());
                Ok((active, completions, submission_id, true))
            }
            AcpSessionState::Closed => Err(invalid_params("ACP session is closed")),
        }
    }

    async fn cancel(&self, notification: CancelNotification) {
        let session_id = notification.session_id.to_string();
        if let Some(session) = self.sessions.lock().await.get(&session_id).cloned()
            && let Some(submission_id) = Self::request_prompt_cancellation(&session).await
            && let Some(active) = active_session(&session).await
        {
            active.agent.cancel_submission(submission_id).await;
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

    async fn request_prompt_cancellation(session: &AcpSession) -> Option<u64> {
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

    async fn insert_active_session(
        &self,
        agent: AgentHandle,
        spec: AgentSpec,
        connection: &ConnectionTo<AcpClient>,
        replay: bool,
    ) -> agent_client_protocol::Result<Arc<AcpActiveSession>> {
        let session_id = agent.session_id().to_string();
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session_id) {
            return Err(invalid_params(format!(
                "ACP session {session_id} is already active"
            )));
        }
        let projection = match ProjectionHandle::start(&agent, connection.clone(), replay).await {
            Ok(projection) => projection,
            Err(error) => {
                drop(sessions);
                agent.close().await;
                return Err(internal_error(error));
            }
        };
        let active = Arc::new(AcpActiveSession {
            agent,
            spec,
            projection,
        });
        let session = Arc::new(AcpSession {
            id: session_id.clone(),
            cwd: active.spec.working_directory.clone(),
            created_at: Utc::now(),
            state: Mutex::new(AcpSessionState::Active(active.clone())),
            prompt: Mutex::new(AcpPromptState::default()),
        });
        sessions.insert(session_id, session);
        Ok(active)
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
            close_acp_session(&session, false).await;
        }
    }
}

const MODEL_CONFIG_ID: &str = "model";
const THINKING_CONFIG_ID: &str = "thought_level";

async fn pending_config_options(
    project: &AcpProject,
    spec: &AgentSpec,
) -> Vec<SessionConfigOption> {
    let models = selectable_models(project, spec).await;
    let selected_model = model_value(&spec.provider, &spec.model);
    let mut grouped = BTreeMap::<String, Vec<SessionConfigSelectOption>>::new();
    for model in &models {
        let name = if model.name.trim().is_empty() {
            model.id.clone()
        } else {
            model.name.clone()
        };
        let mut option =
            SessionConfigSelectOption::new(model_value(&model.provider, &model.id), name);
        if model.name.trim().is_empty() || model.name == model.id {
            option = option.description(format!("{}/{}", model.provider, model.id));
        } else {
            option = option.description(model.id.clone());
        }
        grouped
            .entry(model.provider.clone())
            .or_default()
            .push(option);
    }
    if !models
        .iter()
        .any(|model| model.provider == spec.provider && model.id == spec.model)
    {
        let provider = if spec.provider.is_empty() {
            "URI Agent".to_string()
        } else {
            spec.provider.clone()
        };
        let name = if spec.model.is_empty() {
            "Not configured".to_string()
        } else {
            spec.model.clone()
        };
        grouped.entry(provider).or_default().push(
            SessionConfigSelectOption::new(selected_model.clone(), name)
                .description("Current default; select an authenticated model before prompting"),
        );
    }
    let model_groups = grouped
        .into_iter()
        .map(|(provider, mut options)| {
            options.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.value.0.cmp(&right.value.0))
            });
            SessionConfigSelectGroup::new(format!("provider:{provider}"), provider, options)
        })
        .collect::<Vec<_>>();
    let model = project.catalog.model(&spec.provider, &spec.model).await;
    vec![
        SessionConfigOption::select(MODEL_CONFIG_ID, "Model", selected_model, model_groups)
            .category(SessionConfigOptionCategory::Model),
        thinking_config_option(spec.thinking, model.as_ref(), false),
    ]
}

async fn selectable_models(project: &AcpProject, spec: &AgentSpec) -> Vec<CatalogModel> {
    let providers = project
        .manager
        .model_providers_with_credentials(&spec.provider)
        .await;
    let mut models = Vec::new();
    for provider in providers {
        models.extend(project.catalog.models(&provider).await);
    }
    if !spec.provider.is_empty()
        && !spec.model.is_empty()
        && !models
            .iter()
            .any(|model| model.provider == spec.provider && model.id == spec.model)
        && let Some(current) = project.catalog.model(&spec.provider, &spec.model).await
    {
        models.push(current);
    }
    models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    models
}

async fn selectable_model(
    pending: &AcpPendingSession,
    provider: &str,
    model: &str,
) -> Option<CatalogModel> {
    let providers = pending
        .project
        .manager
        .model_providers_with_credentials(&pending.spec.provider)
        .await;
    if !providers.contains(provider) {
        return None;
    }
    pending
        .project
        .catalog
        .models(provider)
        .await
        .into_iter()
        .find(|candidate| candidate.id == model)
}

fn frozen_config_options(spec: &AgentSpec) -> Vec<SessionConfigOption> {
    let model_name = if spec.model.is_empty() {
        "Not configured".to_string()
    } else {
        spec.model.clone()
    };
    let selected_model = model_value(&spec.provider, &spec.model);
    vec![
        SessionConfigOption::select(
            MODEL_CONFIG_ID,
            "Model",
            selected_model.clone(),
            vec![SessionConfigSelectOption::new(selected_model, model_name)],
        )
        .category(SessionConfigOptionCategory::Model),
        thinking_config_option(spec.thinking, None, true),
    ]
}

fn thinking_config_option(
    current: ThinkingLevel,
    model: Option<&CatalogModel>,
    frozen: bool,
) -> SessionConfigOption {
    let levels = if frozen {
        vec![current]
    } else if let Some(model) = model {
        let mut levels = ThinkingLevel::ALL
            .into_iter()
            .filter(|level| model.supports_thinking_level(*level))
            .collect::<Vec<_>>();
        if !levels.contains(&current) {
            levels.push(current);
            levels.sort_by_key(|level| {
                ThinkingLevel::ALL
                    .iter()
                    .position(|candidate| candidate == level)
                    .unwrap_or_default()
            });
        }
        levels
    } else {
        vec![current]
    };
    let options = levels
        .into_iter()
        .map(|level| {
            let value = level.to_string();
            SessionConfigSelectOption::new(value.clone(), value)
        })
        .collect::<Vec<_>>();
    SessionConfigOption::select(
        THINKING_CONFIG_ID,
        "Thinking level",
        current.to_string(),
        options,
    )
    .category(SessionConfigOptionCategory::ThoughtLevel)
}

fn model_value(provider: &str, model: &str) -> String {
    serde_json::to_string(&[provider, model]).expect("model IDs are serializable")
}

fn decode_model_value(value: &str) -> agent_client_protocol::Result<(String, String)> {
    let [provider, model] = serde_json::from_str::<[String; 2]>(value)
        .map_err(|_| invalid_params("invalid ACP model option value"))?;
    Ok((provider, model))
}

fn validate_frozen_config_value(
    spec: &AgentSpec,
    config_id: &str,
    value: &str,
) -> agent_client_protocol::Result<()> {
    let unchanged = match config_id {
        MODEL_CONFIG_ID => {
            let (provider, model) = decode_model_value(value)?;
            provider == spec.provider && model == spec.model
        }
        THINKING_CONFIG_ID => value
            .parse::<ThinkingLevel>()
            .is_ok_and(|thinking| thinking == spec.thinking),
        _ => return Err(invalid_params("unknown ACP session configuration option")),
    };
    if unchanged {
        Ok(())
    } else {
        Err(invalid_params(
            "session configuration is frozen after the first prompt",
        ))
    }
}

async fn active_session(session: &AcpSession) -> Option<Arc<AcpActiveSession>> {
    match &*session.state.lock().await {
        AcpSessionState::Active(active) => Some(active.clone()),
        AcpSessionState::Pending(_) | AcpSessionState::Closed => None,
    }
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
                let headers = unique_http_headers(
                    server
                        .headers
                        .into_iter()
                        .map(|header| (header.name, header.value)),
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

fn unique_http_headers(
    pairs: impl IntoIterator<Item = (String, String)>,
) -> agent_client_protocol::Result<BTreeMap<String, String>> {
    let mut mapped = BTreeMap::new();
    for (name, value) in pairs {
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid_params(format!("invalid MCP HTTP header name {name:?}")))?;
        let canonical = header.as_str().to_string();
        if mapped.insert(canonical, value).is_some() {
            return Err(invalid_params(format!(
                "MCP HTTP header {name:?} is duplicated"
            )));
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

#[derive(Clone, Debug)]
enum ProjectionProgress {
    Through(Option<u64>),
    Failed(String),
}

struct ProjectionHandle {
    cancel: CancellationToken,
    progress: watch::Receiver<ProjectionProgress>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ProjectionHandle {
    async fn start(
        agent: &AgentHandle,
        connection: ConnectionTo<AcpClient>,
        replay: bool,
    ) -> Result<Self> {
        let native_session = agent.services().runtime.session().clone();
        let mut updates = native_session.subscribe();
        let events = native_session.snapshot().await?;
        let through = events.last().map(|event| event.sequence);
        let mut projector = if replay {
            let mut projector = EventProjector::replay(
                agent.session_id().to_string(),
                connection,
                agent.services().context_window,
            );
            for event in &events {
                projector.persisted(event)?;
            }
            projector.finish_replay();
            projector
        } else {
            EventProjector::live(
                agent.session_id().to_string(),
                connection,
                agent.services().context_window,
                cumulative_cost(&events),
                through,
            )
        };
        let (progress_tx, progress) = watch::channel(ProjectionProgress::Through(through));
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                let update = tokio::select! {
                    _ = task_cancel.cancelled() => return,
                    update = updates.recv() => update,
                };
                match update {
                    Ok(update) => {
                        let persisted = matches!(update, NativeSessionUpdate::Persisted(_));
                        if let Err(error) = projector.update(update) {
                            progress_tx.send_replace(ProjectionProgress::Failed(format!(
                                "cannot project ACP session update: {error:#}"
                            )));
                            return;
                        }
                        if persisted {
                            progress_tx.send_replace(ProjectionProgress::Through(
                                projector.through_sequence,
                            ));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let events = match native_session.snapshot().await {
                            Ok(events) => events,
                            Err(error) => {
                                progress_tx.send_replace(ProjectionProgress::Failed(format!(
                                    "cannot recover lagged ACP session updates: {error:#}"
                                )));
                                return;
                            }
                        };
                        for event in &events {
                            if let Err(error) = projector.persisted(event) {
                                progress_tx.send_replace(ProjectionProgress::Failed(format!(
                                    "cannot recover lagged ACP session updates: {error:#}"
                                )));
                                return;
                            }
                        }
                        progress_tx
                            .send_replace(ProjectionProgress::Through(projector.through_sequence));
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(Self {
            cancel,
            progress,
            task: Mutex::new(Some(task)),
        })
    }

    async fn wait_through(&self, target: Option<u64>) -> Result<()> {
        let Some(target) = target else {
            return Ok(());
        };
        let mut progress = self.progress.clone();
        loop {
            match progress.borrow().clone() {
                ProjectionProgress::Through(Some(through)) if through >= target => return Ok(()),
                ProjectionProgress::Through(_) => {}
                ProjectionProgress::Failed(error) => bail!(error),
            }
            progress
                .changed()
                .await
                .context("ACP session projection stopped before the turn settled")?;
        }
    }

    async fn stop(&self) {
        self.cancel.cancel();
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }
}

async fn close_acp_session(session: &AcpSession, cancel: bool) {
    let active = {
        let mut state = session.state.lock().await;
        match std::mem::replace(&mut *state, AcpSessionState::Closed) {
            AcpSessionState::Active(active) => Some(active),
            AcpSessionState::Pending(_) | AcpSessionState::Closed => None,
        }
    };
    if let Some(active) = active {
        close_active_session(&active, cancel).await;
    }
}

async fn close_active_session(session: &AcpActiveSession, cancel: bool) {
    if cancel {
        session.agent.cancel().await;
    }
    session.agent.close().await;
    let through = session
        .agent
        .services()
        .runtime
        .session()
        .head_sequence()
        .await;
    let _ = session.projection.wait_through(through).await;
    session.projection.stop().await;
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
    through_sequence: Option<u64>,
}

impl EventProjector {
    fn live(
        session_id: String,
        connection: ConnectionTo<AcpClient>,
        context_window: usize,
        cost: f64,
        through_sequence: Option<u64>,
    ) -> Self {
        Self::new(
            session_id,
            connection,
            ProjectionMode::Live,
            context_window,
            cost,
            through_sequence,
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
            None,
        )
    }

    fn new(
        session_id: String,
        connection: ConnectionTo<AcpClient>,
        mode: ProjectionMode,
        context_window: usize,
        cost: f64,
        through_sequence: Option<u64>,
    ) -> Self {
        Self {
            session_id,
            connection,
            mode,
            context_window: context_window as u64,
            cost,
            through_sequence,
        }
    }

    fn finish_replay(&mut self) {
        self.mode = ProjectionMode::Live;
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
        if self
            .through_sequence
            .is_some_and(|through| event.sequence <= through)
        {
            return Ok(());
        }
        let projected = match &event.kind {
            EventKind::User { text } if matches!(self.mode, ProjectionMode::Replay) => {
                self.send(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(text)),
                )))
            }
            EventKind::ModelMessage { message } if matches!(self.mode, ProjectionMode::Replay) => {
                self.user_images(message)
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
        };
        if projected.is_ok() {
            self.through_sequence = Some(event.sequence);
        }
        projected
    }

    fn user_images(&self, message: &Message) -> Result<()> {
        let Message::User { content } = message else {
            return Ok(());
        };
        for content in content {
            let UserContent::Image(image) = content else {
                continue;
            };
            if let Some(image) = acp_image_content(image) {
                self.send(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::Image(image),
                )))?;
            }
        }
        Ok(())
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

fn acp_image_content(image: &rig::message::Image) -> Option<ImageContent> {
    let data = match &image.data {
        DocumentSourceKind::Base64(data) => data.clone(),
        DocumentSourceKind::Raw(data) => BASE64.encode(data),
        _ => return None,
    };
    let mime_type = match image.media_type.as_ref() {
        Some(ImageMediaType::JPEG) => "image/jpeg",
        Some(ImageMediaType::PNG) => "image/png",
        Some(ImageMediaType::GIF) => "image/gif",
        Some(ImageMediaType::WEBP) => "image/webp",
        Some(ImageMediaType::HEIC) => "image/heic",
        Some(ImageMediaType::HEIF) => "image/heif",
        Some(ImageMediaType::SVG) => "image/svg+xml",
        None => {
            let bytes = BASE64.decode(&data).ok()?;
            match ProtocolImageMediaType::detect(&bytes)? {
                ProtocolImageMediaType::Jpeg => "image/jpeg",
                ProtocolImageMediaType::Png => "image/png",
                ProtocolImageMediaType::Gif => "image/gif",
                ProtocolImageMediaType::Webp => "image/webp",
            }
        }
    };
    Some(ImageContent::new(data, mime_type))
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
        v1::{
            EnvVariable, McpServerStdio, ResourceLink, SessionConfigKind,
            SessionConfigSelectOptions,
        },
    };
    use clap::Parser;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn model_project(config_directory: &Path, project_directory: &Path) -> Arc<AcpProject> {
        let manager = ConfigManager::load_for_test(config_directory, project_directory)
            .await
            .unwrap();
        let catalog = Arc::new(ModelCatalog::load(config_directory, true).await.unwrap());
        let environment = Arc::new(
            crate::config::AgentEnvironment::load(config_directory)
                .await
                .unwrap(),
        );
        let cwd = tokio::fs::canonicalize(project_directory).await.unwrap();
        let host = AgentHost::new(manager.clone(), environment, catalog.clone(), cwd.clone())
            .await
            .unwrap();
        Arc::new(AcpProject {
            cwd,
            manager,
            catalog,
            host,
        })
    }

    async fn model_state() -> (tempfile::TempDir, Arc<AcpV1State>, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let config_directory = root.path().join("config");
        let project_directory = root.path().join("project");
        tokio::fs::create_dir_all(&config_directory).await.unwrap();
        tokio::fs::create_dir_all(&project_directory).await.unwrap();
        tokio::fs::write(
            config_directory.join("models.json"),
            br#"{"providers":{"acp-alpha":{"baseUrl":"http://127.0.0.1:9/v1","api":"openai-responses","apiKey":"alpha-test-key","models":[{"id":"alpha-model","name":"Alpha","reasoning":true}]},"acp-beta":{"baseUrl":"http://127.0.0.1:9/v1","api":"openai-responses","apiKey":"beta-test-key","models":[{"id":"beta/model","name":"Beta"}]}}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            config_directory.join("settings.json"),
            br#"{"defaultProvider":"acp-alpha","defaultModel":"alpha-model","defaultThinkingLevel":"high"}"#,
        )
        .await
        .unwrap();
        let project = model_project(&config_directory, &project_directory).await;
        let project_directory = project.cwd.clone();
        let cli = Cli::try_parse_from(["uri-agent", "--acpv1", "--offline"]).unwrap();
        let state = Arc::new(AcpV1State::new(cli));
        state
            .projects
            .lock()
            .await
            .insert(project_directory.clone(), project);
        (root, state, project_directory)
    }

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

    #[tokio::test]
    async fn one_process_keeps_multiple_projects_independent() {
        let (root, state, first_directory) = model_state().await;
        let config_directory = root.path().join("config");
        let second_directory = root.path().join("second-project");
        tokio::fs::create_dir_all(second_directory.join(".uri-agent"))
            .await
            .unwrap();
        tokio::fs::write(
            second_directory.join(".uri-agent/settings.json"),
            br#"{"defaultProvider":"acp-beta","defaultModel":"beta/model","defaultThinkingLevel":"off"}"#,
        )
        .await
        .unwrap();
        let second_project = model_project(&config_directory, &second_directory).await;
        let second_directory = second_project.cwd.clone();
        state
            .projects
            .lock()
            .await
            .insert(second_directory.clone(), second_project.clone());

        let first_project = state.project(&first_directory).await.unwrap();
        assert!(!Arc::ptr_eq(&first_project, &second_project));
        assert!(!Arc::ptr_eq(
            &first_project.manager,
            &second_project.manager
        ));
        let (first, second) = tokio::join!(
            state.new_session(NewSessionRequest::new(&first_directory)),
            state.new_session(NewSessionRequest::new(&second_directory)),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let first_id = first.session_id.to_string();
        let second_id = second.session_id.to_string();
        {
            let first = state.session(&first_id).await.unwrap();
            let state = first.state.lock().await;
            let AcpSessionState::Pending(pending) = &*state else {
                panic!("first project session was unexpectedly materialized");
            };
            assert_eq!(pending.spec.provider, "acp-alpha");
            assert_eq!(pending.spec.model, "alpha-model");
            assert_eq!(pending.spec.working_directory, first_directory);
        }
        {
            let second = state.session(&second_id).await.unwrap();
            let state = second.state.lock().await;
            let AcpSessionState::Pending(pending) = &*state else {
                panic!("second project session was unexpectedly materialized");
            };
            assert_eq!(pending.spec.provider, "acp-beta");
            assert_eq!(pending.spec.model, "beta/model");
            assert_eq!(pending.spec.working_directory, second_directory);
        }

        let first_list = state
            .list_sessions(ListSessionsRequest::new().cwd(&first_directory))
            .await
            .unwrap();
        assert_eq!(first_list.sessions.len(), 1);
        assert_eq!(first_list.sessions[0].session_id.to_string(), first_id);
        assert_eq!(first_list.sessions[0].cwd, first_directory);
        let second_list = state
            .list_sessions(ListSessionsRequest::new().cwd(&second_directory))
            .await
            .unwrap();
        assert_eq!(second_list.sessions.len(), 1);
        assert_eq!(second_list.sessions[0].session_id.to_string(), second_id);
        assert_eq!(second_list.sessions[0].cwd, second_directory);
        let all = state
            .list_sessions(ListSessionsRequest::new())
            .await
            .unwrap();
        assert_eq!(
            all.sessions
                .into_iter()
                .map(|session| (session.session_id.to_string(), session.cwd))
                .collect::<HashSet<_>>(),
            HashSet::from([
                (first_id.clone(), first_directory),
                (second_id.clone(), second_directory),
            ])
        );

        state
            .close_session(CloseSessionRequest::new(first_id))
            .await
            .unwrap();
        state
            .close_session(CloseSessionRequest::new(second_id))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn new_sessions_select_models_in_memory_until_the_first_prompt() {
        let (_root, state, project_directory) = model_state().await;
        let project = state
            .projects
            .lock()
            .await
            .get(&project_directory)
            .cloned()
            .unwrap();
        let defaults = project.manager.current().await;
        let settings_path = project.manager.directory().join("settings.json");
        let original_settings = tokio::fs::read(&settings_path).await.unwrap();

        let first = state
            .new_session(NewSessionRequest::new(&project_directory))
            .await
            .unwrap();
        let second = state
            .new_session(NewSessionRequest::new(&project_directory))
            .await
            .unwrap();
        let first_id = first.session_id.to_string();
        let second_id = second.session_id.to_string();
        let options = first.config_options.as_ref().unwrap();
        assert_eq!(options.len(), 2);
        assert!(matches!(options[0].kind, SessionConfigKind::Select(_)));
        assert!(
            Session::persisted_spec(&project_directory, &first_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            Session::list_for(&project_directory)
                .await
                .unwrap()
                .is_empty()
        );
        let listed = state
            .list_sessions(ListSessionsRequest::new())
            .await
            .unwrap();
        assert_eq!(
            listed
                .sessions
                .iter()
                .map(|session| session.session_id.to_string())
                .collect::<HashSet<_>>(),
            HashSet::from([first_id.clone(), second_id.clone()])
        );

        let selected = state
            .set_config_option(SetSessionConfigOptionRequest::new(
                first_id.clone(),
                MODEL_CONFIG_ID,
                model_value("acp-beta", "beta/model"),
            ))
            .await
            .unwrap();
        assert_eq!(selected.config_options.len(), 2);
        let first_session = state.session(&first_id).await.unwrap();
        let second_session = state.session(&second_id).await.unwrap();
        {
            let first_state = first_session.state.lock().await;
            let AcpSessionState::Pending(first_pending) = &*first_state else {
                panic!("first ACP session was materialized before prompting");
            };
            assert_eq!(first_pending.spec.provider, "acp-beta");
            assert_eq!(first_pending.spec.model, "beta/model");
            assert_eq!(first_pending.spec.thinking, ThinkingLevel::Off);
        }
        {
            let second_state = second_session.state.lock().await;
            let AcpSessionState::Pending(second_pending) = &*second_state else {
                panic!("second ACP session was materialized before prompting");
            };
            assert_eq!(second_pending.spec.provider, defaults.provider);
            assert_eq!(second_pending.spec.model, defaults.model);
        }
        assert_eq!(project.manager.current().await.provider, defaults.provider);
        assert_eq!(project.manager.current().await.model, defaults.model);
        assert_eq!(
            tokio::fs::read(&settings_path).await.unwrap(),
            original_settings
        );

        first_session.prompt.lock().await.begin().unwrap();
        assert!(
            state
                .set_config_option(SetSessionConfigOptionRequest::new(
                    first_id.clone(),
                    MODEL_CONFIG_ID,
                    model_value("acp-beta", "beta/model"),
                ))
                .await
                .is_err()
        );
        first_session.prompt.lock().await.finish();

        state
            .close_session(CloseSessionRequest::new(first_id.clone()))
            .await
            .unwrap();
        state
            .close_session(CloseSessionRequest::new(second_id))
            .await
            .unwrap();
        assert!(
            Session::persisted_spec(&project_directory, &first_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .list_sessions(ListSessionsRequest::new())
                .await
                .unwrap()
                .sessions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn first_prompt_materializes_the_reserved_session_and_freezes_its_model() {
        let (_root, state, project_directory) = model_state().await;
        let (client_transport, agent_transport) = Channel::duplex();
        let agent = tokio::spawn(serve_state_on(state, agent_transport));

        AcpClient
            .builder()
            .on_receive_notification(
                async move |_notification: SessionNotification, _connection| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(NewSessionRequest::new(&project_directory))
                    .block_task()
                    .await?;
                let session_id = created.session_id.to_string();
                connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id.clone(),
                        MODEL_CONFIG_ID,
                        model_value("acp-beta", "beta/model"),
                    ))
                    .block_task()
                    .await?;

                let prompt = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("persist this"))],
                    ))
                    .block_task();
                tokio::pin!(prompt);
                let _ = tokio::select! {
                    result = &mut prompt => result,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                        connection.send_notification(CancelNotification::new(session_id.clone()))?;
                        prompt.await
                    }
                };

                let spec = Session::persisted_spec(&project_directory, &session_id)
                    .await
                    .unwrap()
                    .expect("first ACP prompt did not persist its reserved session");
                assert_eq!(spec.provider, "acp-beta");
                assert_eq!(spec.model, "beta/model");
                assert_eq!(spec.thinking, ThinkingLevel::Off);
                assert!(
                    connection
                        .send_request(SetSessionConfigOptionRequest::new(
                            session_id.clone(),
                            MODEL_CONFIG_ID,
                            model_value("acp-alpha", "alpha-model"),
                        ))
                        .block_task()
                        .await
                        .is_err()
                );
                connection
                    .send_request(CloseSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                let loaded = connection
                    .send_request(LoadSessionRequest::new(
                        session_id.clone(),
                        &project_directory,
                    ))
                    .block_task()
                    .await?;
                let loaded_options = loaded.config_options.unwrap();
                let SessionConfigKind::Select(model) = &loaded_options[0].kind else {
                    panic!("model configuration must be a select option");
                };
                let SessionConfigSelectOptions::Ungrouped(options) = &model.options else {
                    panic!("frozen model options must be ungrouped");
                };
                assert_eq!(options.len(), 1);
                assert_eq!(
                    model.current_value.0.as_ref(),
                    model_value("acp-beta", "beta/model")
                );
                connection
                    .send_request(CloseSessionRequest::new(session_id))
                    .block_task()
                    .await?;
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
        assert_eq!(prompt.request_cancellation(), None);
        prompt.begin().unwrap();
        assert!(prompt.begin().is_err());
        assert_eq!(prompt.request_cancellation(), None);
        assert!(prompt.cancellation_requested);
        assert!(prompt.submitted(42));
        assert_eq!(prompt.request_cancellation(), Some(42));
        prompt.finish();
        assert!(!prompt.active);
        assert!(!prompt.cancellation_requested);
        assert_eq!(prompt.submission_id, None);
    }

    #[test]
    fn initialize_falls_back_to_the_latest_supported_protocol_version() {
        let cli = Cli::try_parse_from(["uri-agent", "--acpv1", "--offline"]).unwrap();
        let request: InitializeRequest = serde_json::from_value(serde_json::json!({
            "protocolVersion": 2,
            "clientCapabilities": {},
            "clientInfo": {"name": "future-client", "version": "1"},
        }))
        .unwrap();

        let response = AcpV1State::new(cli).initialize(request);

        assert_eq!(response.protocol_version, ProtocolVersion::V1);
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
    fn persisted_user_images_map_back_to_acp_image_content() {
        let encoded = BASE64.encode(b"\x89PNG\r\n\x1a\npersisted-image");
        let UserContent::Image(image) =
            UserContent::image_base64(encoded.clone(), Some(ImageMediaType::PNG), None)
        else {
            unreachable!();
        };

        let mapped = acp_image_content(&image).unwrap();

        assert_eq!(mapped.data, encoded);
        assert_eq!(mapped.mime_type, "image/png");
    }

    #[test]
    fn http_header_mapping_rejects_case_insensitive_duplicates() {
        let error = unique_http_headers([
            ("Authorization".to_string(), "first".to_string()),
            ("authorization".to_string(), "second".to_string()),
        ])
        .unwrap_err();

        assert!(format!("{error:?}").contains("duplicated"));
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
