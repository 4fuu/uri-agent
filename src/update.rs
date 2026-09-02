use anyhow::{Context, Result};
use std::time::Duration;

const LATEST_RELEASE_URL: &str = "https://github.com/4fuu/uri-agent/releases/latest";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReleaseVersion(u64, u64, u64);

impl ReleaseVersion {
    fn parse(value: &str) -> Option<Self> {
        let value = value.strip_prefix('v').unwrap_or(value);
        let mut parts = value.split('.');
        let year = parts.next()?.parse().ok()?;
        let month_day = parts.next()?.parse().ok()?;
        let revision = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self(year, month_day, revision))
    }
}

pub(crate) async fn available_version() -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .user_agent(concat!("uri-agent/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = client
        .head(LATEST_RELEASE_URL)
        .send()
        .await?
        .error_for_status()?;
    let tag = response
        .url()
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .context("latest URI Agent release URL has no tag")?;
    Ok(newer_release_version(env!("CARGO_PKG_VERSION"), tag))
}

fn newer_release_version(current: &str, latest_tag: &str) -> Option<String> {
    let current = ReleaseVersion::parse(current)?;
    let latest = ReleaseVersion::parse(latest_tag)?;
    (latest > current).then(|| {
        latest_tag
            .strip_prefix('v')
            .unwrap_or(latest_tag)
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_calendar_release_is_available() {
        assert_eq!(
            newer_release_version("2026.831.0", "v2026.901.0").as_deref(),
            Some("2026.901.0")
        );
        assert_eq!(
            newer_release_version("2026.831.0", "v2026.831.1").as_deref(),
            Some("2026.831.1")
        );
    }

    #[test]
    fn current_older_and_malformed_releases_are_ignored() {
        assert_eq!(newer_release_version("2026.831.0", "v2026.831.0"), None);
        assert_eq!(newer_release_version("2026.831.0", "v2026.830.4"), None);
        assert_eq!(newer_release_version("2026.831.0", "latest"), None);
        assert_eq!(newer_release_version("development", "v2026.901.0"), None);
    }
}
