//! Background jobs: new version on GitHub, first call from an AI client and connections
//! that started needing a reconnect. Only the rules live here; the `shell` schedules them.

use adsconsole_mcp::engine::Engine;
use serde::Deserialize;

use super::connections::{ConnStatusView, ConnectionView};
use super::error::AppError;
use super::runtime::Runtime;

pub const GITHUB_API: &str = "https://api.github.com";

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim_start_matches('v').split('.');
    let n = |s: Option<&str>| s.and_then(|x| x.parse::<u64>().ok());
    let out = (n(it.next())?, n(it.next())?, n(it.next())?);
    it.next().is_none().then_some(out)
}

/// The tag's version, if it is stable and newer than the current one. Pre-releases
/// (`-rc.1`) and tags not in the `vX.Y.Z` format are ignored.
pub fn newer_release(current: &str, latest_tag: &str) -> Option<String> {
    let latest = parse(latest_tag)?;
    let cur = parse(current)?;
    (latest > cur).then(|| format!("{}.{}.{}", latest.0, latest.1, latest.2))
}

/// Queries the latest published release. Any failure (network, 404, JSON) becomes `None`.
pub async fn check_update(
    http: &reqwest::Client,
    api_base: &str,
    repo: &str,
    current: &str,
) -> Option<UpdateInfo> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
        html_url: String,
    }
    let resp = http
        .get(format!("{api_base}/repos/{repo}/releases/latest"))
        .header("User-Agent", "AdsConsole")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let r: Release = resp.json().await.ok()?;
    Some(UpdateInfo {
        version: newer_release(current, &r.tag_name)?,
        url: r.html_url,
    })
}

/// Sets `client_seen` on the first authorized call to the MCP. Returns `true` only the
/// time it set it.
pub async fn first_call_tick(rt: &Runtime) -> Result<bool, AppError> {
    if rt.config().client_seen || rt.last_request_at().await.is_none() {
        return Ok(false);
    }
    rt.update_config(|c| c.client_seen = true)?;
    Ok(true)
}

/// Names of the connections that were fine in `before` and need to reconnect in `after`.
pub fn newly_needing_reconnect(before: &[ConnectionView], after: &[ConnectionView]) -> Vec<String> {
    after
        .iter()
        .filter(|c| c.status == ConnStatusView::NeedsReconnect)
        .filter(|c| {
            before
                .iter()
                .find(|b| b.id == c.id)
                .is_some_and(|b| b.status != ConnStatusView::NeedsReconnect)
        })
        .map(|c| c.name.clone())
        .collect()
}

/// Refreshes the account cache (which also flags connections that lost access).
pub async fn refresh_all(engines: [&Engine; 2]) {
    for e in engines {
        if let Err(err) = e.refresh_accounts().await {
            tracing::debug!(error = %err.message, "failed to refresh accounts in the background");
        }
    }
}
