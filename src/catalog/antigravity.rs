//! Antigravity's account-scoped, authoritative model roster.
//!
//! The built-in Antigravity rows describe a fixed snapshot; accounts without
//! a bundled model receive `404 Requested entity was not found` on every
//! request for it. A successful `fetchAvailableModels` roster filters the
//! built-in list by availability, while failures keep it untouched.

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
        .filter(|(id, _)| !id.ends_with("-disabled") && !id.ends_with("_disabled"))
        .filter(|(_, record)| {
            record.get("disabled").and_then(Value::as_bool) != Some(true)
                && record.get("isInternal").and_then(Value::as_bool) != Some(true)
        })
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
    // Thinking-level models additionally pin their advertised tier so the
    // transport sends `thinkingLevel` instead of a numeric budget.
    let identity_route = match roster_thinking_level(id, record) {
        Some(level) => json!({"model": id, "thinkingLevel": level}),
        None => json!({"model": id}),
    };
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

/// The tier a roster record advertises for models controlled by
/// `thinkingLevel` instead of a numeric budget. Google publishes the tier as
/// a string or a number; only new-generation families (Gemini 4+, Claude 5+)
/// take the level channel, because 2.5-era records reuse the field while
/// staying budget-controlled.
fn roster_thinking_level(id: &str, record: &Value) -> Option<&'static str> {
    if !uses_thinking_level_channel(id) {
        return None;
    }
    match record.get("thinkingLevel")? {
        Value::String(level) => match level.trim().to_ascii_uppercase().as_str() {
            "LOW" | "1" => Some("LOW"),
            "MEDIUM" | "2" => Some("MEDIUM"),
            "HIGH" | "3" => Some("HIGH"),
            _ => None,
        },
        Value::Number(level) => match level.as_i64() {
            Some(1) => Some("LOW"),
            Some(2) => Some("MEDIUM"),
            Some(3) => Some("HIGH"),
            _ => None,
        },
        _ => None,
    }
}

fn uses_thinking_level_channel(id: &str) -> bool {
    let version = id
        .strip_prefix("gemini-")
        .and_then(|rest| {
            rest.split(|character: char| !character.is_ascii_digit() && character != '.')
                .next()
        })
        .and_then(|version| version.parse::<f32>().ok());
    if version.is_some_and(|version| version >= 4.0) {
        return true;
    }
    // `claude-<family>-<major>-<minor>[-tier]`, e.g. claude-opus-5-5-high.
    id.strip_prefix("claude-")
        .and_then(|rest| rest.split('-').nth(1))
        .and_then(|minor| minor.parse::<u32>().ok())
        .is_some_and(|minor| minor >= 5)
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
/// discovered rows. Accounts that carry only part of a model's effort tiers
/// keep the logical row, but the effort levels whose route targets the
/// roster lacks are marked unsupported in its `thinkingLevelMap`, so they
/// can no longer be selected into a request that would 404. Rows without an
/// enumerated map keep their default support rules. An empty successful
/// roster therefore removes every built-in row, while a failed lookup never
/// reaches this function and keeps the built-in list.
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
    // Mark effort levels whose route target the account lacks as
    // unsupported. Appended discovered rows route every level at a roster
    // id, so this only ever rewrites surviving built-in rows.
    for value in models.values_mut() {
        let selectable_levels = route_targets_of(value)
            .map(|routes| {
                routes
                    .iter()
                    .filter(|(_, route)| {
                        route
                            .get("model")
                            .and_then(Value::as_str)
                            .is_some_and(|target| available.contains(target))
                    })
                    .map(|(level, _)| level.clone())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let Some(levels) = value
            .get_mut("thinkingLevelMap")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        for mapped in levels.values_mut() {
            if let Some(level) = mapped.as_str()
                && !selectable_levels.contains(level)
            {
                *mapped = Value::Null;
            }
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
                "internal-model": {"isInternal": true},
                "grey-model-disabled": {"supportsThinking": true},
                "retired-model": {"disabled": true, "supportsThinking": true}
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
    fn tiered_roster_models_route_a_thinking_level() {
        let models = parse_roster(json!({
            "models": {
                "gemini-4-argon-high": {"supportsThinking": true, "thinkingLevel": 3},
                "claude-opus-5-5-low": {"supportsThinking": true, "thinkingLevel": "1"},
                "gemini-4-argon": {"supportsThinking": true, "thinkingLevel": "MEDIUM"},
                // Budget-controlled families ignore the field entirely.
                "gemini-2.5-flash": {"supportsThinking": true, "thinkingLevel": 3},
                "claude-sonnet-4-6": {"supportsThinking": true, "thinkingLevel": 3},
                "gemini-4-argon-tiered": {"supportsThinking": true, "thinkingLevel": 9}
            }
        }))
        .unwrap();
        let route_level = |id: &str, level: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .and_then(|model| model.compat("antigravityRoutes"))
                .and_then(|routes| routes.get(level))
                .and_then(|route| route.get("thinkingLevel"))
                .and_then(Value::as_str)
        };
        assert_eq!(route_level("gemini-4-argon-high", "high"), Some("HIGH"));
        assert_eq!(route_level("claude-opus-5-5-low", "low"), Some("LOW"));
        assert_eq!(route_level("gemini-4-argon", "medium"), Some("MEDIUM"));
        // Budget-controlled families ignore the field entirely.
        assert_eq!(route_level("gemini-2.5-flash", "high"), None);
        assert_eq!(route_level("claude-sonnet-4-6", "high"), None);
        // Unknown tier numbers do not invent a level channel.
        assert_eq!(route_level("gemini-4-argon-tiered", "high"), None);
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
                    "medium": {"model": "gemini-3.1-pro-high", "thinkingBudget": 10001}
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
                "gemini-3.1-pro-high": {"supportsThinking": true},
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

    #[test]
    fn partially_available_models_lose_only_the_missing_effort_levels() {
        let mut models = BTreeMap::from([(
            "gemini-x".to_string(),
            json!({
                "id": "gemini-x",
                "name": "Gemini X",
                "api": "antigravity",
                "provider": PROVIDER,
                "baseUrl": "https://cloudcode-pa.googleapis.com",
                "reasoning": true,
                "thinkingLevelMap": {
                    "off": null, "minimal": null, "low": "low", "medium": "medium",
                    "high": "high", "xhigh": null, "max": "max"
                },
                "compat": {"antigravityRoutes": {
                    "low": {"model": "gemini-x-low", "thinkingBudget": 1000},
                    "medium": {"model": "gemini-x-pro", "thinkingBudget": 4000},
                    "high": {"model": "gemini-x-pro", "thinkingBudget": 10000},
                    "max": {"model": "gemini-x-max", "thinkingBudget": 20000}
                }}
            }),
        )]);
        // Only the low tier's target is in the account roster.
        let roster =
            parse_roster(json!({"models": {"gemini-x-low": {"supportsThinking": true}}})).unwrap();
        merge_roster(&mut models, &roster);

        assert_eq!(models.len(), 1);
        let levels = models["gemini-x"]["thinkingLevelMap"].clone();
        assert_eq!(levels["low"], "low");
        assert_eq!(levels["medium"], Value::Null);
        assert_eq!(levels["high"], Value::Null);
        assert_eq!(levels["max"], Value::Null);
        // The surviving route keeps its tuned budget untouched.
        let routes = models["gemini-x"]["compat"]["antigravityRoutes"].clone();
        assert_eq!(routes["low"]["thinkingBudget"], 1000);
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
