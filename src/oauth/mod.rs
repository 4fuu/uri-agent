mod callback;
mod device;
mod providers;
mod util;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::{mpsc, oneshot, watch};

pub(crate) use providers::{
    CODEBUDDY_ACCOUNT_EXTRA, CODEBUDDY_DOMAIN_EXTRA, CODEBUDDY_ENDPOINT_EXTRA,
    CODEBUDDY_ENVIRONMENT_EXTRA, CODEBUDDY_METHOD_EXTRA, chatgpt_account_id,
    codebuddy_default_endpoint, normalize_codebuddy_endpoint,
};
pub use util::parse_authorization_input;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OauthProvider {
    Antigravity,
    Anthropic,
    CodeBuddy,
    OpenRouter,
    OpenAiCodex,
    GitHubCopilot,
    KimiCoding,
    Xai,
    Radius,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OauthMethod {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

impl OauthProvider {
    pub const ALL: [Self; 9] = [
        Self::Antigravity,
        Self::Anthropic,
        Self::CodeBuddy,
        Self::OpenRouter,
        Self::OpenAiCodex,
        Self::GitHubCopilot,
        Self::KimiCoding,
        Self::Xai,
        Self::Radius,
    ];

    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "antigravity" => Self::Antigravity,
            "anthropic" => Self::Anthropic,
            "codebuddy" => Self::CodeBuddy,
            "openrouter" => Self::OpenRouter,
            "openai-codex" => Self::OpenAiCodex,
            "github-copilot" => Self::GitHubCopilot,
            "kimi-coding" => Self::KimiCoding,
            "xai" => Self::Xai,
            "radius" => Self::Radius,
            _ => return None,
        })
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::Anthropic => "anthropic",
            Self::CodeBuddy => "codebuddy",
            Self::OpenRouter => "openrouter",
            Self::OpenAiCodex => "openai-codex",
            Self::GitHubCopilot => "github-copilot",
            Self::KimiCoding => "kimi-coding",
            Self::Xai => "xai",
            Self::Radius => "radius",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Antigravity => "Google Antigravity",
            Self::Anthropic => "Anthropic (Claude Pro/Max)",
            Self::CodeBuddy => "CodeBuddy",
            Self::OpenRouter => "OpenRouter OAuth",
            Self::OpenAiCodex => "OpenAI Codex",
            Self::GitHubCopilot => "GitHub Copilot",
            Self::KimiCoding => "Kimi Code (subscription)",
            Self::Xai => "xAI (Grok/X subscription)",
            Self::Radius => "Radius",
        }
    }

    pub fn methods(self) -> &'static [OauthMethod] {
        match self {
            Self::Antigravity => &[OauthMethod {
                id: "oauth",
                label: "Experimental Google OAuth",
                description: "Private, unsupported Antigravity protocol",
            }],
            Self::Anthropic => &[OauthMethod {
                id: "oauth",
                label: "Claude Pro/Max",
                description: "Browser OAuth, same flow as Pi Agent",
            }],
            Self::CodeBuddy => &[
                OauthMethod {
                    id: "internal",
                    label: "Chinese Site",
                    description: "Sign in with WeChat",
                },
                OauthMethod {
                    id: "external",
                    label: "International Site",
                    description: "Sign in with Google or GitHub",
                },
                OauthMethod {
                    id: "selfhosted",
                    label: "Enterprise Domain",
                    description: "Sign in through an enterprise CodeBuddy endpoint",
                },
            ],
            Self::OpenRouter => &[OauthMethod {
                id: "oauth",
                label: "Sign in with OpenRouter",
                description: "Browser PKCE that mints a user-controlled API key",
            }],
            Self::OpenAiCodex => &[
                OauthMethod {
                    id: "browser",
                    label: "Browser login",
                    description: "OpenAI account login in the browser",
                },
                OauthMethod {
                    id: "device_code",
                    label: "Device code",
                    description: "Headless login with a user code",
                },
            ],
            Self::GitHubCopilot => &[OauthMethod {
                id: "oauth",
                label: "GitHub Copilot",
                description: "Device code; optional GitHub Enterprise domain",
            }],
            Self::KimiCoding => &[OauthMethod {
                id: "oauth",
                label: "Sign in with Kimi Code",
                description: "Subscription device-code login",
            }],
            Self::Xai => &[OauthMethod {
                id: "oauth",
                label: "Sign in with SuperGrok or X Premium",
                description: "Device-code subscription login",
            }],
            Self::Radius => &[
                OauthMethod {
                    id: "browser",
                    label: "Browser login",
                    description: "Sign in at the Radius gateway",
                },
                OauthMethod {
                    id: "device_code",
                    label: "Device code",
                    description: "When the browser is on another machine",
                },
            ],
        }
    }

    pub fn offers_api_key(self) -> bool {
        !matches!(self, Self::Antigravity | Self::OpenAiCodex)
    }
}

pub fn oauth_enabled(provider: &str) -> bool {
    OauthProvider::from_id(provider).is_some()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OauthToken {
    #[serde(rename = "type")]
    pub kind: String,
    pub refresh: String,
    pub access: String,
    pub expires: i64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl OauthToken {
    pub fn from_response(access: String, refresh: String, expires_in: i64) -> Self {
        Self {
            kind: "oauth".to_string(),
            refresh,
            access,
            expires: chrono::Utc::now().timestamp_millis() + expires_in * 1000 - 5 * 60 * 1000,
            extra: BTreeMap::new(),
        }
    }

    pub fn with_extra(mut self, extra: BTreeMap<String, Value>) -> Self {
        self.extra = extra;
        self
    }

    pub fn expired(&self) -> bool {
        chrono::Utc::now().timestamp_millis() >= self.expires
    }
}

#[derive(Clone)]
pub struct OauthDisplay {
    pub url: String,
    pub user_code: Option<String>,
    pub instructions: String,
}

pub struct OauthLogin {
    display: std::sync::Arc<std::sync::Mutex<OauthDisplay>>,
    paste: mpsc::Sender<String>,
    cancel: watch::Sender<bool>,
}

impl OauthLogin {
    pub fn display(&self) -> OauthDisplay {
        self.display
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn submit_paste(&self, input: &str) {
        let _ = self.paste.try_send(input.to_string());
    }

    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

impl Drop for OauthLogin {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}

pub fn start_login(
    provider: &str,
    method: &str,
    extra: &BTreeMap<String, String>,
) -> Result<(OauthLogin, oneshot::Receiver<Result<OauthToken>>)> {
    let Some(kind) = OauthProvider::from_id(provider) else {
        bail!("provider {provider} has no OAuth login");
    };
    let method = if method.is_empty() {
        kind.methods()[0].id
    } else {
        method
    };
    match kind {
        OauthProvider::Antigravity => providers::start_antigravity(),
        OauthProvider::Anthropic => providers::start_anthropic(),
        OauthProvider::CodeBuddy => {
            providers::start_codebuddy(method, extra.get("endpoint").map(String::as_str))
        }
        OauthProvider::OpenRouter => providers::start_openrouter(),
        OauthProvider::OpenAiCodex if method == "device_code" => providers::start_codex_device(),
        OauthProvider::OpenAiCodex => providers::start_codex_browser(),
        OauthProvider::GitHubCopilot => providers::start_github_copilot(extra.get("domain")),
        OauthProvider::KimiCoding => providers::start_kimi(),
        OauthProvider::Xai => providers::start_xai(),
        OauthProvider::Radius if method == "device_code" => {
            providers::start_radius_device(extra.get("gateway").map(String::as_str))
        }
        OauthProvider::Radius => {
            providers::start_radius_browser(extra.get("gateway").map(String::as_str))
        }
    }
}

pub async fn refresh_token(provider: &str, token: &OauthToken) -> Result<OauthToken> {
    let Some(kind) = OauthProvider::from_id(provider) else {
        bail!("provider {provider} has no OAuth refresh");
    };
    let refreshed = match kind {
        OauthProvider::Antigravity => providers::refresh_antigravity(token).await,
        OauthProvider::Anthropic => providers::refresh_anthropic(&token.refresh).await,
        OauthProvider::CodeBuddy => providers::refresh_codebuddy(token).await,
        OauthProvider::OpenRouter => Ok(token.clone()),
        OauthProvider::OpenAiCodex => providers::refresh_codex(&token.refresh).await,
        OauthProvider::GitHubCopilot => providers::refresh_github_copilot(token).await,
        OauthProvider::KimiCoding => providers::refresh_kimi(&token.refresh).await,
        OauthProvider::Xai => providers::refresh_xai(&token.refresh).await,
        OauthProvider::Radius => providers::refresh_radius(token).await,
    }?;
    normalize_refreshed_token(kind, token, refreshed)
}

fn normalize_refreshed_token(
    provider: OauthProvider,
    previous: &OauthToken,
    mut refreshed: OauthToken,
) -> Result<OauthToken> {
    if provider != OauthProvider::OpenRouter && refreshed.refresh.is_empty() {
        if previous.refresh.is_empty() {
            bail!(
                "{} OAuth refresh returned no refresh token and the stored credential has none",
                provider.name()
            );
        }
        refreshed.refresh.clone_from(&previous.refresh);
    }
    Ok(refreshed)
}

pub(super) struct LoginSetup {
    pub login: OauthLogin,
    pub paste_rx: mpsc::Receiver<String>,
    pub cancel_rx: watch::Receiver<bool>,
    pub done_tx: oneshot::Sender<Result<OauthToken>>,
    pub done_rx: oneshot::Receiver<Result<OauthToken>>,
    pub display: std::sync::Arc<std::sync::Mutex<OauthDisplay>>,
}

pub(super) fn channels(
    url: String,
    user_code: Option<String>,
    instructions: impl Into<String>,
) -> LoginSetup {
    let (paste_tx, paste_rx) = mpsc::channel(1);
    let (done_tx, done_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let display = std::sync::Arc::new(std::sync::Mutex::new(OauthDisplay {
        url,
        user_code,
        instructions: instructions.into(),
    }));
    LoginSetup {
        login: OauthLogin {
            display: display.clone(),
            paste: paste_tx,
            cancel: cancel_tx,
        },
        paste_rx,
        cancel_rx,
        done_tx,
        done_rx,
        display,
    }
}

pub(super) fn set_display(
    display: &std::sync::Arc<std::sync::Mutex<OauthDisplay>>,
    url: impl Into<String>,
    user_code: Option<String>,
    instructions: impl Into<String>,
) {
    if let Ok(mut display) = display.lock() {
        display.url = url.into();
        display.user_code = user_code;
        display.instructions = instructions.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_oauth_providers_are_registered() {
        assert_eq!(OauthProvider::ALL.len(), 9);
        assert!(oauth_enabled("antigravity"));
        assert!(oauth_enabled("anthropic"));
        assert!(oauth_enabled("codebuddy"));
        assert!(oauth_enabled("openrouter"));
        assert!(oauth_enabled("openai-codex"));
        assert!(oauth_enabled("github-copilot"));
        assert!(oauth_enabled("kimi-coding"));
        assert!(oauth_enabled("xai"));
        assert!(oauth_enabled("radius"));
        assert!(!oauth_enabled("openai"));
        assert_eq!(OauthProvider::Antigravity.name(), "Google Antigravity");
        assert_eq!(OauthProvider::CodeBuddy.methods().len(), 3);
        assert_eq!(OauthProvider::OpenAiCodex.methods().len(), 2);
        assert!(!OauthProvider::OpenAiCodex.offers_api_key());
        assert!(!OauthProvider::Antigravity.offers_api_key());
    }

    #[test]
    fn oauth_token_expiry_is_skewed_five_minutes_early() {
        let token = OauthToken::from_response("a".into(), "r".into(), 3600);
        assert!(!token.expired());
        assert!(token.expires <= chrono::Utc::now().timestamp_millis() + 55 * 60 * 1000);
    }

    #[test]
    fn oauth_token_expires_at_its_stored_boundary() {
        let token = OauthToken {
            kind: "oauth".into(),
            access: "access".into(),
            refresh: "refresh".into(),
            expires: chrono::Utc::now().timestamp_millis(),
            extra: BTreeMap::new(),
        };
        assert!(token.expired());
    }

    #[test]
    fn refresh_normalization_retains_or_rotates_refresh_token() {
        let previous = OauthToken {
            kind: "oauth".into(),
            access: "old-access".into(),
            refresh: "old-refresh".into(),
            expires: 1,
            extra: BTreeMap::new(),
        };
        let omitted = OauthToken {
            kind: "oauth".into(),
            access: "new-access".into(),
            refresh: String::new(),
            expires: 2,
            extra: BTreeMap::new(),
        };
        let normalized =
            normalize_refreshed_token(OauthProvider::KimiCoding, &previous, omitted).unwrap();
        assert_eq!(normalized.refresh, "old-refresh");

        let rotated = OauthToken {
            refresh: "rotated-refresh".into(),
            ..normalized
        };
        let normalized =
            normalize_refreshed_token(OauthProvider::KimiCoding, &previous, rotated).unwrap();
        assert_eq!(normalized.refresh, "rotated-refresh");
    }

    #[test]
    fn refresh_normalization_rejects_nonrenewable_credentials() {
        let token = OauthToken {
            kind: "oauth".into(),
            access: "access".into(),
            refresh: String::new(),
            expires: 1,
            extra: BTreeMap::new(),
        };
        let error = normalize_refreshed_token(OauthProvider::KimiCoding, &token, token.clone())
            .unwrap_err();
        assert!(error.to_string().contains("stored credential has none"));

        assert!(
            normalize_refreshed_token(OauthProvider::OpenRouter, &token, token.clone()).is_ok()
        );
    }
}
