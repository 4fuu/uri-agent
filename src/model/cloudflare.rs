//! Cloudflare AI Gateway's provider-owned request boundary.
//!
//! The merged catalog remains authoritative for model identity and capability
//! metadata: API family, modalities, limits, thinking compatibility, and cost.
//! It is deliberately *not* authoritative for this provider's endpoint,
//! authentication headers, or wire model ID. Those security-sensitive values
//! are derived here from Cloudflare's public REST contract and structured user
//! credentials, so a pi.dev catalog change cannot redirect a Cloudflare token
//! or turn it into an upstream provider credential.
//!
//! Keep future provider integrations with a similar split in their own model
//! module instead of accumulating provider branches in the generic Rig backend
//! or request transformer. The generic layer should receive resolved request
//! options, not know why one provider needs them.

#[cfg(test)]
use super::rig_backend::AuthClient;
use super::rig_backend::{RigBackend, RigRequestOptions};
use super::{ModelBackend, ModelDelta, ModelRequest, ModelResponse};
use crate::catalog::CatalogModel;
use crate::config::{ActiveSettings, AuthKind, ConfigManager};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Url;
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

pub(crate) const PROVIDER: &str = "cloudflare-ai-gateway";
pub(crate) const ACCOUNT_ID_METADATA: &str = "accountId";
pub(crate) const GATEWAY_ID_METADATA: &str = "gatewayId";
pub(crate) const ACCOUNT_ID_ENVIRONMENT: &str = "CLOUDFLARE_ACCOUNT_ID";
pub(crate) const GATEWAY_ID_ENVIRONMENT: &str = "CLOUDFLARE_GATEWAY_ID";
pub(crate) const DEFAULT_GATEWAY_ID: &str = "default";
const REST_ORIGIN: &str = "https://api.cloudflare.com/client/v4";

#[derive(Clone, Eq, PartialEq)]
struct CloudflareCredential {
    token: String,
    account_id: String,
    gateway_id: String,
}

pub(super) struct CloudflareBackend {
    model: CatalogModel,
    settings: ActiveSettings,
    session_id: Option<String>,
    manager: Arc<ConfigManager>,
    backend: Mutex<Option<(CloudflareCredential, Arc<RigBackend>)>>,
}

impl CloudflareBackend {
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

    async fn backend(&self) -> Result<Arc<RigBackend>> {
        let credential = resolve_credential(&self.manager, &self.settings).await?;
        let mut cached = self.backend.lock().await;
        if let Some((cached_credential, backend)) = cached.as_ref()
            && cached_credential == &credential
        {
            return Ok(backend.clone());
        }
        let (model, options) = request_model(&self.model, &credential)?;
        let backend = Arc::new(
            RigBackend::new_with_options(
                &model,
                &credential.token,
                &self.settings.credential_environment,
                AuthKind::ApiKey,
                self.settings.thinking,
                self.session_id.as_deref(),
                options,
            )
            .await?,
        );
        *cached = Some((credential, backend.clone()));
        Ok(backend)
    }
}

#[async_trait]
impl ModelBackend for CloudflareBackend {
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

async fn resolve_credential(
    manager: &ConfigManager,
    settings: &ActiveSettings,
) -> Result<CloudflareCredential> {
    let token = manager
        .resolve_model_api_key(settings)
        .await?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Cloudflare AI Gateway requires a Cloudflare API token"))?;
    let metadata = manager.credential_metadata(PROVIDER).await;
    let account_id = environment_or_metadata(ACCOUNT_ID_ENVIRONMENT, &metadata, ACCOUNT_ID_METADATA)
        .ok_or_else(|| {
            anyhow!(
                "Cloudflare AI Gateway requires an account ID; run :login again or set {ACCOUNT_ID_ENVIRONMENT}"
            )
        })?;
    let gateway_id =
        environment_or_metadata(GATEWAY_ID_ENVIRONMENT, &metadata, GATEWAY_ID_METADATA)
            .unwrap_or_else(|| DEFAULT_GATEWAY_ID.to_string());
    Ok(CloudflareCredential {
        token,
        account_id,
        gateway_id,
    })
}

fn environment_or_metadata(
    environment: &str,
    metadata: &BTreeMap<String, Value>,
    key: &str,
) -> Option<String> {
    env::var(environment)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            metadata
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

fn request_model(
    catalog: &CatalogModel,
    credential: &CloudflareCredential,
) -> Result<(CatalogModel, RigRequestOptions)> {
    if catalog.provider != PROVIDER {
        bail!(
            "Cloudflare backend cannot run provider {}",
            catalog.provider
        );
    }
    if !matches!(
        catalog.api.as_str(),
        "openai-responses" | "openai-completions" | "anthropic-messages"
    ) {
        bail!(
            "Cloudflare AI Gateway does not support catalog API {:?} through its REST backend",
            catalog.api
        );
    }

    let mut model = catalog.clone();
    model.base_url = rest_base_url(&credential.account_id)?;
    model.id = wire_model_id(catalog)?;
    // Catalog headers and authHeader are transport configuration, not model
    // capability metadata. This backend owns and replaces both by contract.
    model.headers.clear();
    model.metadata.remove("authHeader");
    apply_request_compat_defaults(&mut model);

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", credential.token))
            .context("invalid Cloudflare API token for Authorization header")?,
    );
    headers.insert(
        HeaderName::from_static("cf-aig-gateway-id"),
        HeaderValue::from_str(&credential.gateway_id)
            .context("invalid Cloudflare AI Gateway ID")?,
    );
    Ok((
        model,
        RigRequestOptions {
            extra_headers: headers,
            // Rig's Anthropic client normally adds x-api-key. Cloudflare REST
            // authenticates the account with Authorization instead; retaining
            // x-api-key would make the account token look like an upstream key.
            strip_x_api_key: catalog.api == "anthropic-messages",
        },
    ))
}

fn apply_request_compat_defaults(model: &mut CatalogModel) {
    if model.api != "openai-completions" {
        return;
    }
    let compat = model
        .metadata
        .entry("compat".to_string())
        .or_insert_with(|| Value::Object(Default::default()));
    let Some(compat) = compat.as_object_mut() else {
        return;
    };
    // These are Cloudflare REST fallback decisions, not generic provider
    // heuristics. Explicit catalog compat values remain authoritative.
    compat.entry("supportsStore").or_insert(Value::Bool(false));
    compat
        .entry("maxTokensField")
        .or_insert_with(|| Value::String("max_tokens".to_string()));
    compat
        .entry("supportsStrictMode")
        .or_insert(Value::Bool(false));
    compat
        .entry("supportsReasoningEffort")
        .or_insert(Value::Bool(false));
}

fn rest_base_url(account_id: &str) -> Result<String> {
    let mut url = Url::parse(REST_ORIGIN).expect("Cloudflare REST origin is valid");
    url.path_segments_mut()
        .map_err(|_| anyhow!("Cloudflare REST origin cannot accept path segments"))?
        .extend(["accounts", account_id, "ai", "v1"]);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn wire_model_id(model: &CatalogModel) -> Result<String> {
    if let Some(workers) = model.id.strip_prefix("workers-ai/") {
        if !workers.starts_with("@cf/") {
            bail!("Cloudflare Workers AI model ID must contain the @cf/ namespace");
        }
        return Ok(workers.to_string());
    }
    if model.id.starts_with("@cf/") || model.id.contains('/') {
        return Ok(model.id.clone());
    }
    let namespace = match model.api.as_str() {
        "anthropic-messages" => "anthropic",
        "openai-responses" | "openai-completions" => "openai",
        _ => bail!("Cloudflare model has unsupported API {:?}", model.api),
    };
    Ok(format!("{namespace}/{}", model.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::Request;
    use serde_json::json;

    fn model(id: &str, api: &str) -> CatalogModel {
        let mut metadata = BTreeMap::new();
        metadata.insert("authHeader".to_string(), Value::Bool(true));
        metadata.insert("contextWindow".to_string(), json!(1_000_000));
        CatalogModel {
            id: id.to_string(),
            name: id.to_string(),
            api: api.to_string(),
            provider: PROVIDER.to_string(),
            base_url: "https://catalog.example/credential-leak".to_string(),
            headers: BTreeMap::from([(
                "x-catalog-header".to_string(),
                "must-not-survive".to_string(),
            )]),
            metadata,
        }
    }

    fn credential() -> CloudflareCredential {
        CloudflareCredential {
            token: "cloudflare-test-token".to_string(),
            account_id: "account-123".to_string(),
            gateway_id: "gateway-one".to_string(),
        }
    }

    #[test]
    fn catalog_transport_fields_are_replaced_but_capabilities_survive() {
        let (runtime, options) = request_model(
            &model("claude-sonnet-4-6", "anthropic-messages"),
            &credential(),
        )
        .unwrap();

        assert_eq!(
            runtime.base_url,
            "https://api.cloudflare.com/client/v4/accounts/account-123/ai/v1"
        );
        assert_eq!(runtime.id, "anthropic/claude-sonnet-4-6");
        assert!(runtime.headers.is_empty());
        assert!(!runtime.metadata.contains_key("authHeader"));
        assert_eq!(runtime.context_window(), 1_000_000);
        assert!(options.strip_x_api_key);
        assert_eq!(
            options.extra_headers["cf-aig-gateway-id"],
            HeaderValue::from_static("gateway-one")
        );
        assert_eq!(
            options.extra_headers[http::header::AUTHORIZATION],
            HeaderValue::from_static("Bearer cloudflare-test-token")
        );
    }

    #[test]
    fn openai_and_workers_ai_ids_use_cloudflare_rest_namespaces() {
        let (openai, options) =
            request_model(&model("gpt-5.4", "openai-responses"), &credential()).unwrap();
        let (workers, _) = request_model(
            &model("workers-ai/@cf/zai-org/glm-5.3-flash", "openai-completions"),
            &credential(),
        )
        .unwrap();

        assert_eq!(openai.id, "openai/gpt-5.4");
        assert!(!options.strip_x_api_key);
        assert_eq!(workers.id, "@cf/zai-org/glm-5.3-flash");
        assert_eq!(workers.compat("supportsStore"), Some(&Value::Bool(false)));
        assert_eq!(
            workers.compat("maxTokensField").and_then(Value::as_str),
            Some("max_tokens")
        );
    }

    #[test]
    fn account_id_is_encoded_as_one_url_path_segment() {
        assert_eq!(
            rest_base_url("account/with/slashes").unwrap(),
            "https://api.cloudflare.com/client/v4/accounts/account%2Fwith%2Fslashes/ai/v1"
        );
    }

    #[test]
    fn anthropic_account_token_is_not_sent_as_x_api_key() {
        let client = AuthClient {
            inner: reqwest::Client::new(),
            strip_x_api_key: true,
            transform: None,
            codex_websocket: None,
            antigravity: None,
        };
        let request = Request::builder()
            .uri("https://api.cloudflare.com/test")
            .header("x-api-key", "cloudflare-test-token")
            .header("authorization", "Bearer cloudflare-test-token")
            .body(Bytes::new())
            .unwrap();

        let prepared = client.prepare(request);

        assert!(!prepared.headers().contains_key("x-api-key"));
        assert_eq!(
            prepared.headers()[http::header::AUTHORIZATION],
            HeaderValue::from_static("Bearer cloudflare-test-token")
        );
    }

    #[test]
    fn unsupported_catalog_api_is_rejected_before_a_request() {
        let error = request_model(
            &model("gemini-3-flash", "google-generative-ai"),
            &credential(),
        )
        .err()
        .expect("unsupported API should fail");
        assert!(error.to_string().contains("google-generative-ai"));
    }
}
