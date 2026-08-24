use chrono::{DateTime, Utc};
use http::HeaderMap;
use rig::completion::CompletionError;
use std::fmt;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ModelFailureKind {
    ContextOverflow,
    RateLimit,
    Timeout,
    Network,
    Server,
    Conflict,
    EmptyResponse,
    Authentication,
    Quota,
    Client,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelFailurePhase {
    Request,
    Stream,
    Response,
}

#[derive(Debug)]
pub(crate) struct ModelFailure {
    kind: ModelFailureKind,
    phase: ModelFailurePhase,
    status: Option<http::StatusCode>,
    retry_after: Option<Duration>,
    provider_request_id: Option<String>,
    message: String,
}

impl ModelFailure {
    pub(super) fn from_completion_error(error: CompletionError, phase: ModelFailurePhase) -> Self {
        let status = error.provider_response_status();
        let headers = error.provider_response_headers();
        let retry_after = parse_retry_after(headers);
        let provider_request_id = error
            .provider_request_id()
            .map(str::to_string)
            .or_else(|| request_id_from_headers(headers));
        let message = error.to_string();
        let diagnostic = error.provider_response_body().unwrap_or(&message);
        let kind = classify_model_failure(&error, status, diagnostic);
        Self {
            kind,
            phase,
            status,
            retry_after,
            provider_request_id,
            message,
        }
    }

    pub(crate) fn empty_response() -> Self {
        Self {
            kind: ModelFailureKind::EmptyResponse,
            phase: ModelFailurePhase::Response,
            status: None,
            retry_after: None,
            provider_request_id: None,
            message: "model returned no assistant content".to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        kind: ModelFailureKind,
        retry_after: Option<Duration>,
        message: &str,
    ) -> Self {
        Self {
            kind,
            phase: ModelFailurePhase::Request,
            status: None,
            retry_after,
            provider_request_id: None,
            message: message.to_string(),
        }
    }

    pub(crate) fn kind(&self) -> ModelFailureKind {
        self.kind
    }

    pub(crate) fn phase(&self) -> ModelFailurePhase {
        self.phase
    }

    pub(crate) fn status(&self) -> Option<http::StatusCode> {
        self.status
    }

    pub(crate) fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub(crate) fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
}

impl fmt::Display for ModelFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let phase = match self.phase {
            ModelFailurePhase::Request => "request",
            ModelFailurePhase::Stream => "stream",
            ModelFailurePhase::Response => "response",
        };
        write!(formatter, "model {phase} failed: {}", self.message)?;
        if let Some(request_id) = self
            .provider_request_id
            .as_deref()
            .filter(|request_id| !self.message.contains(request_id))
        {
            write!(formatter, " (request id: {request_id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ModelFailure {}

fn classify_model_failure(
    error: &CompletionError,
    status: Option<http::StatusCode>,
    diagnostic: &str,
) -> ModelFailureKind {
    let diagnostic = diagnostic.to_ascii_lowercase();
    if contains_any(
        &diagnostic,
        &[
            "insufficient_quota",
            "quota exceeded",
            "usage limit reached",
            "monthly usage limit",
            "available balance",
            "out of budget",
            "billing",
        ],
    ) {
        return ModelFailureKind::Quota;
    }
    if status.is_none_or(|status| status.is_success()) {
        if contains_any(
            &diagnostic,
            &[
                "rate_limit_error",
                "rate_limit_exceeded",
                "too_many_requests",
                "resource_exhausted",
                "rate limit",
                "too many requests",
            ],
        ) {
            return ModelFailureKind::RateLimit;
        }
        if contains_any(
            &diagnostic,
            &[
                "overloaded_error",
                "server_error",
                "internal_server_error",
                "service_unavailable",
                "api_error",
                "service unavailable",
                "temporarily unavailable",
                "server is overloaded",
            ],
        ) {
            return ModelFailureKind::Server;
        }
    }
    match status {
        Some(http::StatusCode::UNAUTHORIZED | http::StatusCode::FORBIDDEN) => {
            ModelFailureKind::Authentication
        }
        Some(http::StatusCode::REQUEST_TIMEOUT) => ModelFailureKind::Timeout,
        Some(http::StatusCode::CONFLICT) => ModelFailureKind::Conflict,
        Some(http::StatusCode::TOO_MANY_REQUESTS) => ModelFailureKind::RateLimit,
        Some(http::StatusCode::PAYLOAD_TOO_LARGE) => ModelFailureKind::ContextOverflow,
        Some(status) if status.is_server_error() => ModelFailureKind::Server,
        Some(status) if status.is_client_error() => {
            if looks_like_context_overflow(&diagnostic) {
                ModelFailureKind::ContextOverflow
            } else {
                ModelFailureKind::Client
            }
        }
        _ if looks_like_context_overflow(&diagnostic) => ModelFailureKind::ContextOverflow,
        _ if looks_like_timeout(&diagnostic) => ModelFailureKind::Timeout,
        _ if looks_like_network_failure(&diagnostic) => ModelFailureKind::Network,
        _ => match error {
            CompletionError::HttpError(rig::http_client::Error::Instance(_))
            | CompletionError::HttpError(rig::http_client::Error::StreamEnded) => {
                ModelFailureKind::Network
            }
            _ => ModelFailureKind::Other,
        },
    }
}

pub(crate) fn looks_like_context_overflow(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if contains_any(
        &message,
        &[
            "rate limit",
            "too many requests",
            "throttling error",
            "service unavailable",
        ],
    ) {
        return false;
    }
    contains_any(
        &message,
        &[
            "prompt is too long",
            "request_too_large",
            "input is too long for requested model",
            "exceeds the context window",
            "maximum context length",
            "input token count",
            "maximum prompt length",
            "reduce the length of the messages",
            "maximum allowed input length",
            "longer than the model's context length",
            "exceeds the available context size",
            "greater than the context length",
            "context window exceeds limit",
            "exceeded model token limit",
            "model_context_window_exceeded",
            "prompt too long",
            "configured context size",
            "range of input length should be",
            "context_length_exceeded",
            "context length exceeded",
            "too many tokens",
            "token limit exceeded",
        ],
    ) || message.starts_with("400 status code (no body)")
        || message.starts_with("413 status code (no body)")
}

fn looks_like_timeout(message: &str) -> bool {
    contains_any(
        message,
        &[
            "timed out",
            "timeout",
            "deadline exceeded",
            "operation timed out",
        ],
    )
}

fn looks_like_network_failure(message: &str) -> bool {
    contains_any(
        message,
        &[
            "connection lost",
            "connection reset",
            "connection refused",
            "connection closed",
            "socket hang up",
            "broken pipe",
            "network error",
            "network connection",
            "error sending request",
            "error decoding response body",
            "unexpected eof",
            "premature end",
            "stream ended",
            "incomplete message",
            "dns error",
            "failed to lookup address",
            "enotfound",
            "eai_again",
        ],
    )
}

fn contains_any(message: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| message.contains(pattern))
}

fn request_id_from_headers(headers: Option<&HeaderMap>) -> Option<String> {
    let headers = headers?;
    ["x-request-id", "request-id", "x-goog-request-id"]
        .iter()
        .find_map(|name| headers.get(*name)?.to_str().ok())
        .filter(|request_id| !request_id.is_empty())
        .map(str::to_string)
}

fn parse_retry_after(headers: Option<&HeaderMap>) -> Option<Duration> {
    parse_retry_after_at(headers, Utc::now())
}

pub(super) fn parse_retry_after_at(
    headers: Option<&HeaderMap>,
    now: DateTime<Utc>,
) -> Option<Duration> {
    let headers = headers?;
    if let Some(milliseconds) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_nonnegative_number)
    {
        return Duration::try_from_secs_f64(milliseconds / 1_000.0).ok();
    }
    let value = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Some(seconds) = parse_nonnegative_number(value) {
        return Duration::try_from_secs_f64(seconds).ok();
    }
    let requested = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    Some(
        requested
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default(),
    )
}

fn parse_nonnegative_number(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}
