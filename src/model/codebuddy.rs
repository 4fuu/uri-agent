//! WorkBuddy's provider-owned request and authentication boundary.
//!
//! Model capability metadata comes from WorkBuddy's cloud product
//! configuration, while endpoint and authentication fields are resolved here.
//! Catalog or user model overrides must not be able to redirect a WorkBuddy
//! credential or replace its identity headers.

use super::failure::ModelFailurePhase;
use super::rig_backend::{RigBackend, RigRequestOptions};
use super::{ModelBackend, ModelDelta, ModelFailure, ModelRequest, ModelResponse};
use crate::catalog::CatalogModel;
use crate::codebuddy::{
    AUTH_TOKEN_VARIABLE, BASE_URL_VARIABLE, ENVIRONMENT_VARIABLE, Session as CodeBuddySession,
    authenticated_headers, process_session_from, session_from_oauth,
};
use crate::config::{ActiveSettings, AuthKind, ConfigManager, ValueSource};
use crate::oauth::normalize_codebuddy_endpoint;
#[cfg(test)]
use crate::oauth::{CODEBUDDY_ENDPOINT_EXTRA, CODEBUDDY_ENVIRONMENT_EXTRA, OauthToken};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use http::StatusCode;
#[cfg(test)]
use serde_json::Value;
use std::env;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

pub(crate) const PROVIDER: &str = "workbuddy";

#[derive(Clone, PartialEq)]
struct CodeBuddyCredential {
    token: String,
    api_key: bool,
    refreshable: bool,
    session: CodeBuddySession,
}

pub(super) struct CodeBuddyBackend {
    model: CatalogModel,
    settings: ActiveSettings,
    session_id: Option<String>,
    manager: Arc<ConfigManager>,
    backend: Mutex<Option<(CodeBuddyCredential, Arc<RigBackend>)>>,
}

impl CodeBuddyBackend {
    pub(super) fn new(
        model: CatalogModel,
        settings: ActiveSettings,
        session_id: Option<&str>,
        manager: Arc<ConfigManager>,
    ) -> Self {
        Self {
            model,
            settings,
            session_id: session_id.map(str::to_string),
            manager,
            backend: Mutex::new(None),
        }
    }

    async fn backend(&self) -> Result<(Arc<RigBackend>, bool)> {
        let credential = resolve_credential(&self.manager, &self.settings).await?;
        let refreshable = credential.refreshable;
        let mut cached = self.backend.lock().await;
        if let Some((cached_credential, backend)) = cached.as_ref()
            && cached_credential == &credential
        {
            return Ok((backend.clone(), refreshable));
        }
        let (model, options) = request_model(&self.model, &credential)?;
        let backend = Arc::new(
            RigBackend::new_with_options(
                &model,
                &credential.token,
                &self.settings.credential_environment,
                self.settings.auth_kind,
                self.settings.thinking,
                self.session_id.as_deref(),
                options,
            )
            .await?,
        );
        *cached = Some((credential, backend.clone()));
        Ok((backend, refreshable))
    }

    async fn clear_backend(&self) {
        *self.backend.lock().await = None;
    }
}

#[async_trait]
impl ModelBackend for CodeBuddyBackend {
    async fn prepare(&self) -> Result<()> {
        self.backend().await.map(|_| ())
    }

    async fn complete(
        &self,
        request: ModelRequest,
        deltas: mpsc::UnboundedSender<ModelDelta>,
    ) -> Result<ModelResponse> {
        let (backend, refreshable) = self.backend().await?;
        let result = backend.complete(request.clone(), deltas.clone()).await;
        let unauthorized_before_stream = result.as_ref().err().is_some_and(|error| {
            error.downcast_ref::<ModelFailure>().is_some_and(|failure| {
                failure.status() == Some(StatusCode::UNAUTHORIZED)
                    && failure.phase() == ModelFailurePhase::Request
            })
        });
        if !refreshable || !unauthorized_before_stream {
            return result;
        }

        self.manager.force_refresh_oauth(PROVIDER).await?;
        self.clear_backend().await;
        self.backend().await?.0.complete(request, deltas).await
    }

    fn accepts_image_input(&self) -> bool {
        self.model.accepts_input("image")
    }

    fn desired_max_output_tokens(&self) -> usize {
        self.model.max_tokens() as usize
    }
}

async fn resolve_credential(
    manager: &ConfigManager,
    settings: &ActiveSettings,
) -> Result<CodeBuddyCredential> {
    let token = manager
        .resolve_model_api_key(settings)
        .await?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("WorkBuddy requires a login, API key, or auth token"))?;
    let custom_token = matches!(
        &settings.api_key_source,
        ValueSource::Environment(name) if name == AUTH_TOKEN_VARIABLE
    );
    // An API key overrides the bearer token in WorkBuddy but does not discard
    // the current signed-in account identity. A custom auth token is itself a
    // complete non-refreshable session and therefore does replace it.
    let stored = if custom_token {
        None
    } else {
        manager.oauth_token(PROVIDER).await.ok()
    };
    let base_url = env::var(BASE_URL_VARIABLE)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let session = match stored.as_ref() {
        Some(stored) => session_from_oauth(stored, base_url.as_deref())?,
        None => process_session_from(base_url, env::var(ENVIRONMENT_VARIABLE).ok())?,
    };
    let refreshable = settings.auth_kind == AuthKind::Oauth
        && settings.api_key_source == ValueSource::Global
        && stored
            .as_ref()
            .is_some_and(|stored| !stored.refresh.is_empty());
    Ok(CodeBuddyCredential {
        token,
        api_key: settings.auth_kind == AuthKind::ApiKey,
        refreshable,
        session,
    })
}

fn request_model(
    catalog: &CatalogModel,
    credential: &CodeBuddyCredential,
) -> Result<(CatalogModel, RigRequestOptions)> {
    if catalog.provider != PROVIDER {
        bail!("WorkBuddy backend cannot run provider {}", catalog.provider);
    }
    if catalog.api != "openai-completions" {
        bail!(
            "WorkBuddy does not support catalog API {:?} through its model endpoint",
            catalog.api
        );
    }

    let mut model = catalog.clone();
    model.base_url = model_base_url(&credential.session.endpoint)?;
    model.headers.clear();
    for key in ["authHeader", "apiKey", "apiKeyHeader", "url"] {
        model.metadata.remove(key);
    }

    let headers =
        authenticated_headers(&credential.token, credential.api_key, &credential.session)?;
    Ok((
        model,
        RigRequestOptions {
            extra_headers: headers,
            strip_x_api_key: false,
        },
    ))
}

fn model_base_url(endpoint: &str) -> Result<String> {
    let endpoint = normalize_codebuddy_endpoint(endpoint)?;
    if endpoint.ends_with("/v2") {
        Ok(endpoint)
    } else {
        Ok(format!("{endpoint}/v2"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as TEST_BASE64;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn model() -> CatalogModel {
        CatalogModel {
            id: "default-model".to_string(),
            name: "Auto".to_string(),
            api: "openai-completions".to_string(),
            provider: PROVIDER.to_string(),
            base_url: "https://catalog.example/credential-leak".to_string(),
            headers: BTreeMap::from([(
                "x-catalog-header".to_string(),
                "must-not-survive".to_string(),
            )]),
            metadata: BTreeMap::from([
                ("authHeader".to_string(), Value::Bool(true)),
                ("contextWindow".to_string(), json!(176_000)),
            ]),
        }
    }

    fn oauth_credential() -> CodeBuddyCredential {
        CodeBuddyCredential {
            token: "oauth-access".to_string(),
            api_key: false,
            refreshable: true,
            session: CodeBuddySession {
                endpoint: "https://enterprise.example/base".to_string(),
                domain: Some("enterprise.example".to_string()),
                method: Some("github".to_string()),
                account: Some(json!({
                    "uid": "user-1",
                    "enterpriseId": "enterprise-1",
                    "departmentFullName": "engineering/platform",
                    "idSource": "github"
                })),
            },
        }
    }

    #[test]
    fn catalog_transport_is_replaced_and_oauth_identity_is_complete() {
        let (runtime, options) = request_model(&model(), &oauth_credential()).unwrap();

        assert_eq!(runtime.base_url, "https://enterprise.example/base/v2");
        assert!(runtime.headers.is_empty());
        assert!(!runtime.metadata.contains_key("authHeader"));
        assert_eq!(runtime.context_window(), 176_000);
        let headers = options.extra_headers;
        assert_eq!(headers["authorization"], "Bearer oauth-access");
        assert_eq!(headers["x-requested-with"], "XMLHttpRequest");
        assert_eq!(headers["x-product"], "SaaS");
        assert_eq!(headers["user-agent"], crate::oauth::WORKBUDDY_USER_AGENT);
        assert_eq!(headers["x-domain"], "enterprise.example");
        assert_eq!(headers["x-user-id"], "user-1");
        assert_eq!(headers["x-enterprise-id"], "enterprise-1");
        assert_eq!(headers["x-tenant-id"], "enterprise-1");
        assert_eq!(headers["x-department-info"], "engineering/platform");
        assert_eq!(headers["x-auth-method"], "github");
        assert_eq!(headers["x-id-source"], "github");
        assert!(!headers.contains_key("x-api-key"));
        let userinfo: Value = serde_json::from_slice(
            &TEST_BASE64
                .decode(headers["x-userinfo"].as_bytes())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            userinfo,
            json!({
                "uin": "user-1",
                "owner_uin": "enterprise-1",
                "id_source": "github",
                "token_source": "github"
            })
        );
    }

    #[test]
    fn api_keys_are_sent_as_bearer_and_x_api_key() {
        let mut credential = oauth_credential();
        credential.token = "api-key".to_string();
        credential.api_key = true;
        credential.refreshable = false;

        let (_, options) = request_model(&model(), &credential).unwrap();

        assert_eq!(options.extra_headers["authorization"], "Bearer api-key");
        assert_eq!(options.extra_headers["x-api-key"], "api-key");
    }

    #[test]
    fn base_url_accepts_an_existing_v2_suffix() {
        assert_eq!(
            model_base_url("http://localhost:3000/v2/").unwrap(),
            "http://localhost:3000/v2"
        );
    }

    #[test]
    fn process_base_url_overrides_a_saved_oauth_endpoint() {
        let token = OauthToken {
            kind: "oauth".to_string(),
            refresh: "refresh".to_string(),
            access: "access".to_string(),
            expires: i64::MAX,
            extra: BTreeMap::from([
                (
                    CODEBUDDY_ENVIRONMENT_EXTRA.to_string(),
                    Value::String("external".to_string()),
                ),
                (
                    CODEBUDDY_ENDPOINT_EXTRA.to_string(),
                    Value::String("https://www.workbuddy.ai".to_string()),
                ),
            ]),
        };

        let session = session_from_oauth(&token, Some("https://override.example/v2/")).unwrap();

        assert_eq!(session.endpoint, "https://override.example/v2");
    }

    #[test]
    fn ioa_and_unsupported_catalog_apis_are_rejected() {
        assert!(process_session_from(None, Some("iOA".to_string())).is_err());
        assert!(process_session_from(None, Some("internal".to_string())).is_err());
        assert_eq!(
            process_session_from(None, None).unwrap().endpoint,
            "https://www.workbuddy.ai"
        );

        let mut unsupported = model();
        unsupported.api = "openai-responses".to_string();
        assert!(request_model(&unsupported, &oauth_credential()).is_err());
    }
}
