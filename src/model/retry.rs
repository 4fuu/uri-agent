use super::{ModelFailure, ModelFailureKind, ModelFailurePhase};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
pub(crate) struct ModelRetryPolicy {
    pub(crate) max_retries: usize,
    pub(crate) base_delay: Duration,
    pub(crate) max_delay: Duration,
    pub(crate) reason: &'static str,
}

pub(crate) fn model_retry_policy(kind: ModelFailureKind) -> Option<ModelRetryPolicy> {
    match kind {
        ModelFailureKind::RateLimit => Some(ModelRetryPolicy {
            max_retries: 20,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            reason: "rate limit",
        }),
        ModelFailureKind::Network => Some(ModelRetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
            reason: "network error",
        }),
        ModelFailureKind::Server => Some(ModelRetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(15),
            reason: "server error",
        }),
        ModelFailureKind::Timeout => Some(ModelRetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            reason: "timeout",
        }),
        ModelFailureKind::Conflict => Some(ModelRetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
            reason: "request conflict",
        }),
        ModelFailureKind::EmptyResponse => Some(ModelRetryPolicy {
            max_retries: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            reason: "empty response",
        }),
        ModelFailureKind::ContextOverflow
        | ModelFailureKind::Authentication
        | ModelFailureKind::Quota
        | ModelFailureKind::Client
        | ModelFailureKind::Other => None,
    }
}

pub(crate) fn model_retry_delay(
    failure: &ModelFailure,
    policy: ModelRetryPolicy,
    retry: usize,
) -> Duration {
    if let Some(requested) = failure.retry_after() {
        return requested.min(MAX_RETRY_AFTER);
    }
    let multiplier = 1_u32 << retry.saturating_sub(1).min(31);
    let backoff = policy
        .base_delay
        .saturating_mul(multiplier)
        .min(policy.max_delay);
    let jitter_limit_ms = (backoff / 4).as_millis();
    let jitter_ms = if jitter_limit_ms == 0 {
        0
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            % (jitter_limit_ms + 1)
    };
    backoff
        .saturating_add(Duration::from_millis(
            jitter_ms.try_into().unwrap_or(u64::MAX),
        ))
        .min(policy.max_delay)
}

pub(crate) fn model_retry_reason(failure: &ModelFailure, policy: ModelRetryPolicy) -> String {
    let phase = match failure.phase() {
        ModelFailurePhase::Request => "request",
        ModelFailurePhase::Stream => "stream",
        ModelFailurePhase::Response => "response",
    };
    let mut reason = failure.status().map_or_else(
        || format!("{} during {phase}", policy.reason),
        |status| {
            format!(
                "{} during {phase} (HTTP {})",
                policy.reason,
                status.as_u16()
            )
        },
    );
    if let Some(request_id) = failure.provider_request_id() {
        reason.push_str(&format!("; request id {request_id}"));
    }
    reason
}
