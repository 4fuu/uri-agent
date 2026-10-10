//! Antigravity's account-scoped, authoritative model roster.
//!
//! The built-in Antigravity rows describe a fixed snapshot; accounts without
//! a bundled model receive `404 Requested entity was not found` on every
//! request for it. A successful `fetchAvailableModels` roster replaces the
//! built-in list, while failures keep it untouched.

use super::{CatalogCredential, CatalogModel};
use crate::model::{ensure_antigravity_version, generation_user_agent};
use anyhow::{Context, Result, bail};
use http::header::{ACCEPT, AUTHORIZATION};
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub(crate) const PROVIDER: &str = "antigravity";
const DAILY_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
const SANDBOX_ENDPOINT: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
const FETCH_AVAILABLE_MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
const DEFAULT_MAX_TOKENS: u64 = 64_000;
/// Roster entries that are not usable generation models even though the
/// account lists them.
const DENYLIST: &[&str] = &["chat_20706", "chat_23310", "gemini-2.5-pro"];

pub(crate) async fn discover(
    _client: &Client,
    credential: &CatalogCredential,
) -> Result<Vec<CatalogModel>> {
    // Do not allow a roster response to forward the Google credential away
    // from the fixed Cloud Code endpoints.
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DISCOVERY_TIMEOUT)
        .build()?;
    ensure_antigravity_version(&client).await;
    let value = fetch_roster(
        &client,
        &[
            format!("{DAILY_ENDPOINT}{FETCH_AVAILABLE_MODELS_PATH}"),
            format!("{SANDBOX_ENDPOINT}{FETCH_AVAILABLE_MODELS_PATH}"),
        ],
        credential,
    )
    .await?;
    parse_roster(value)
}

async fn fetch_roster(
    client: &Client,
    endpoints: &[String],
    credential: &CatalogCredential,
) -> Result<Value> {
    let mut last_error = None;
    for endpoint in endpoints {
        let request = client
            .post(endpoint)
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", credential.secret))
            .header("Content-Type", "application/json")
            .header("User-Agent", generation_user_agent())
            .body("{}");
        match request
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(response) => {
                return response
                    .json::<Value>()
                    .await
                    .context("Antigravity model roster is not valid JSON");
            }
            Err(error) => last_error = Some(error),
        }
    }
    let error = last_error.expect("one endpoint is always attempted");
    Err(anyhow::Error::new(error).context("Antigravity model roster is unavailable"))
}

fn parse_roster(value: Value) -> Result<Vec<CatalogModel>> {
    let Some(records) = value.get("models").and_then(Value::as_object) else {
        bail!("Antigravity model roster has no models object");
    };
    let models = records
        .iter()
        .filter(|(id, _)| !DENYLIST.contains(&id.as_str()))
        .filter(|(_, record)| record.get("isInternal").and_then(Value::as_bool) != Some(true))
        .map(|(id, record)| (id.clone(), model(id, record)))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    Ok(models)
}

fn model(id: &str, record: &Value) -> CatalogModel {
    let supports_images = record.get("supportsImages").and_then(Value::as_bool) == Some(true);
    let reasoning = record.get("supportsThinking").and_then(Value::as_bool) == Some(true);
    let context_window = positive_or(
        record.get("maxTokens").and_then(Value::as_u64),
        DEFAULT_CONTEXT_WINDOW,
    );
    let max_tokens = positive_or(
        record.get("maxOutputTokens").and_then(Value::as_u64),
        DEFAULT_MAX_TOKENS,
    );
    let name = record
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(id);
    // The roster only names models; route every effort at the roster id
    // itself and let the transport defaults derive budgets and output caps.
    let identity_route = json!({"model": id});
    let mut metadata = BTreeMap::from([
        ("reasoning".to_string(), Value::Bool(reasoning)),
        (
            "input".to_string(),
            if supports_images {
                json!(["text", "image"])
            } else {
                json!(["text"])
            },
        ),
        ("contextWindow".to_string(), Value::from(context_window)),
        ("maxTokens".to_string(), Value::from(max_tokens)),
        ("discovered".to_string(), Value::Bool(true)),
        (
            "compat".to_string(),
            json!({"antigravityRoutes": {
                "off": identity_route,
                "low": identity_route,
                "medium": identity_route,
                "high": identity_route,
                "max": identity_route
            }}),
        ),
    ]);
    if !reasoning {
        metadata.insert(
            "thinkingLevelMap".to_string(),
            json!({"off": "off", "minimal": null, "low": null, "medium": null, "high": null, "xhigh": null, "max": null}),
        );
    }
    CatalogModel {
        id: id.to_string(),
        name: name.to_string(),
        api: "antigravity".to_string(),
        provider: PROVIDER.to_string(),
        base_url: "https://cloudcode-pa.googleapis.com".to_string(),
        headers: BTreeMap::new(),
        metadata,
    }
}

fn positive_or(value: Option<u64>, fallback: u64) -> u64 {
    value.filter(|value| *value > 0).unwrap_or(fallback)
}

fn route_targets_of(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value
        .get("compat")
        .and_then(|compat| compat.get("antigravityRoutes"))
        .and_then(Value::as_object)
}

/// Merge a successful account roster into the provider's raw catalog rows.
///
/// The built-in Antigravity rows are logical models whose per-effort routes
/// target different upstream IDs (with tuned budgets and output caps), so
/// the roster is authoritative for *availability*, not identity: a built-in
/// row survives while any of its route targets remains in the account
/// roster, and roster models that no built-in route targets are appended as
/// discovered rows. An empty successful roster therefore removes every
/// built-in row, while a failed lookup never reaches this function and
/// keeps the built-in list.
pub(super) fn merge_roster(models: &mut BTreeMap<String, Value>, roster: &[CatalogModel]) {
    let available = roster
        .iter()
        .map(|model| model.id.as_str())
        .collect::<BTreeSet<_>>();
    let covered_by_built_ins = models
        .values()
        .filter_map(|value| Some(route_targets_of(value)?.values()))
        .flatten()
        .filter_map(|route| route.get("model").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    models.retain(|_, value| {
        route_targets_of(value).is_none_or(|routes| {
            routes.values().any(|route| {
                route
                    .get("model")
                    .and_then(Value::as_str)
                    .is_some_and(|target| available.contains(target))
            })
        })
    });
    for model in roster {
        if covered_by_built_ins.contains(model.id.as_str()) {
            continue;
        }
        if let Ok(value) = serde_json::to_value(model) {
            models.insert(model.id.clone(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{self, MockResponse};

    fn credential() -> CatalogCredential {
        CatalogCredential {
            secret: "antigravity-access".to_string(),
            oauth: true,
            radius_gateway: None,
            workbuddy: None,
            antigravity_account: None,
        }
    }

    #[test]
    fn roster_filters_internal_and_denied_entries_and_builds_routes() {
        let models = parse_roster(json!({
            "models": {
                "claude-sonnet-4-6": {
                    "displayName": "Claude Sonnet 4.6",
                    "supportsImages": true,
                    "supportsThinking": true,
                    "maxTokens": 200000,
                    "maxOutputTokens": 64000
                },
                "gemini-3.1-pro": {"supportsThinking": true},
                "gemini-2.5-pro": {"supportsThinking": true},
                "chat_20706": {},
                "internal-model": {"isInternal": true}
            }
        }))
        .unwrap();
        assert_eq!(models.len(), 2);
        let sonnet = &models[0];
        assert_eq!(sonnet.id, "claude-sonnet-4-6");
        assert_eq!(sonnet.name, "Claude Sonnet 4.6");
        assert_eq!(sonnet.api, "antigravity");
        assert_eq!(sonnet.provider, PROVIDER);
        assert!(sonnet.reasoning());
        assert!(sonnet.accepts_input("image"));
        assert_eq!(sonnet.max_tokens(), 64_000);
        assert!(sonnet.compat("antigravityRoutes").is_some());
        assert!(sonnet.metadata.contains_key("discovered"));
        let gemini = &models[1];
        assert!(!gemini.accepts_input("image"));
        assert_eq!(gemini.name, "gemini-3.1-pro");
        assert_eq!(
            gemini
                .compat("antigravityRoutes")
                .and_then(|routes| routes.get("off"))
                .and_then(|route| route.get("model"))
                .and_then(Value::as_str),
            Some("gemini-3.1-pro")
        );
    }

    #[test]
    fn non_reasoning_roster_models_only_offer_off() {
        let models = parse_roster(json!({
            "models": {"gemini-3.1-flash-lite": {"supportsThinking": false}}
        }))
        .unwrap();
        let model = &models[0];
        assert!(!model.reasoning());
        assert!(model.supports_thinking_level(crate::catalog::ThinkingLevel::Off));
        assert!(!model.supports_thinking_level(crate::catalog::ThinkingLevel::Low));
    }

    #[test]
    fn empty_roster_is_authoritative() {
        assert!(parse_roster(json!({"models": {}})).unwrap().is_empty());
        assert!(
            parse_roster(json!({"models": {"chat_20706": {}, "internal": {"isInternal": true}}}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_models_object_is_an_error() {
        assert!(parse_roster(json!({"agentModelSorts": []})).is_err());
    }

    fn raw_built_in(id: &str, routes: Value) -> (String, Value) {
        (
            id.to_string(),
            json!({
                "id": id,
                "name": id,
                "api": "antigravity",
                "provider": PROVIDER,
                "baseUrl": "https://cloudcode-pa.googleapis.com",
                "compat": {"antigravityRoutes": routes}
            }),
        )
    }

    #[test]
    fn merge_roster_prunes_unavailable_built_ins_and_appends_uncovered_rows() {
        let mut models = BTreeMap::from([
            raw_built_in(
                "gemini-3.1-pro",
                json!({
                    "low": {"model": "gemini-3.1-pro-low", "thinkingBudget": 1001},
                    "medium": {"model": "gemini-pro-agent", "thinkingBudget": 10001}
                }),
            ),
            raw_built_in(
                "gemini-3.8-flash",
                json!({
                    "low": {"model": "gemini-3.8-flash-low"},
                    "high": {"model": "gemini-3.8-flash-high"}
                }),
            ),
            raw_built_in(
                "claude-sonnet-4-6",
                json!({"low": {"model": "claude-sonnet-4-6"}}),
            ),
        ]);
        let roster = parse_roster(json!({
            "models": {
                "gemini-3.1-pro-low": {"supportsThinking": true},
                "gemini-pro-agent": {"supportsThinking": true},
                "claude-sonnet-4-6": {"supportsThinking": true},
                "brand-new-model": {"supportsThinking": true}
            }
        }))
        .unwrap();
        merge_roster(&mut models, &roster);

        // gemini-3.1-pro survives through its covered route targets with the
        // tuned budgets intact; gemini-3.8-flash has no target left and is
        // pruned; roster rows covered by a built-in route are not duplicated,
        // and brand-new-model is appended as a discovered row.
        assert_eq!(
            models.keys().map(String::as_str).collect::<Vec<_>>(),
            ["brand-new-model", "claude-sonnet-4-6", "gemini-3.1-pro"]
        );
        let routes = models["gemini-3.1-pro"]["compat"]["antigravityRoutes"].clone();
        assert_eq!(routes["low"]["model"], "gemini-3.1-pro-low");
        assert_eq!(routes["low"]["thinkingBudget"], 1001);
        // Metadata is flattened on serialization.
        assert_eq!(models["brand-new-model"]["discovered"], true);
    }

    #[test]
    fn an_empty_successful_roster_removes_every_built_in() {
        let mut models = BTreeMap::from([raw_built_in(
            "gemini-3.1-pro",
            json!({"low": {"model": "gemini-3.1-pro-low"}}),
        )]);
        merge_roster(&mut models, &[]);
        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn roster_request_carries_bearer_and_generation_identity() {
        let server = test_http::serve(vec![MockResponse::json(
            200,
            r#"{"models":{"gemini-3.1-pro":{"supportsThinking":true}}}"#,
        )])
        .await;
        let endpoint = server.base.clone();
        let value = fetch_roster(
            &Client::new(),
            &[format!("{endpoint}{FETCH_AVAILABLE_MODELS_PATH}")],
            &credential(),
        )
        .await
        .unwrap();
        assert_eq!(
            value,
            json!({"models":{"gemini-3.1-pro":{"supportsThinking":true}}})
        );
        let requests = server.requests().await;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /v1internal:fetchAvailableModels"));
        assert!(requests[0].contains("authorization: Bearer antigravity-access"));
        assert!(requests[0].contains("content-type: application/json"));
        assert!(requests[0].contains(&format!("user-agent: {}", generation_user_agent())));
    }

    #[tokio::test]
    async fn daily_failure_falls_back_to_the_sandbox_endpoint() {
        let daily =
            test_http::serve(vec![MockResponse::json(503, r#"{"error":"unavailable"}"#)]).await;
        let sandbox = test_http::serve(vec![MockResponse::json(
            200,
            r#"{"models":{"claude-sonnet-4-6":{"supportsThinking":true}}}"#,
        )])
        .await;
        let value = fetch_roster(
            &Client::new(),
            &[daily.base.clone(), sandbox.base.clone()],
            &credential(),
        )
        .await
        .unwrap();
        assert!(value.get("models").is_some());
        assert_eq!(daily.requests().await.len(), 1);
        assert_eq!(sandbox.requests().await.len(), 1);
    }

    #[tokio::test]
    async fn all_endpoints_failing_reports_the_last_error() {
        let server = test_http::serve(vec![MockResponse::json(500, "{}")]).await;
        let error = fetch_roster(
            &Client::new(),
            &[server.base.clone(), server.base.clone()],
            &credential(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("roster is unavailable"));
    }
}
