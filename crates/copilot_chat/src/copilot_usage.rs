//! Reads the signed-in GitHub Copilot account's usage for the current billing
//! cycle.
//!
//! GitHub does not expose remaining quota through its documented REST surface:
//! `/users/{user}/settings/billing/premium_request/usage` only reports spend
//! after the fact, and the token-exchange endpoint returns null quotas on paid
//! plans. The `/copilot_internal/user` endpoint used here is the same
//! undocumented endpoint the first-party editor integrations call to render
//! their usage badge. It is authenticated with the long-lived OAuth (`ghu_…`)
//! token rather than a short-lived API token.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use serde::Deserialize;

use crate::CopilotChatConfiguration;

/// Version this endpoint is known to accept. It is not the same value the chat
/// endpoints send, and passing an unrecognized version here can fail the
/// request outright.
const GITHUB_API_VERSION: &str = "2025-04-01";

/// The quota bucket GitHub bills as "AI credits" (previously "premium
/// requests"). Free plans instead meter the `chat` and `completions` buckets.
const PREMIUM_INTERACTIONS_QUOTA_ID: &str = "premium_interactions";

/// GitHub reports an unlimited entitlement as `-1` rather than setting the
/// `unlimited` flag on some plans.
const UNLIMITED_ENTITLEMENT: f64 = -1.0;

#[derive(Debug, Clone, PartialEq)]
pub struct CopilotUsage {
    /// Marketing plan name, e.g. `"free"`, `"individual"`, `"business"`.
    pub plan: Option<String>,
    /// Date the quotas reset, as reported by GitHub. May be a bare `YYYY-MM-DD`
    /// or a full RFC 3339 timestamp depending on the plan.
    pub resets_on: Option<String>,
    /// `None` when the account has no metered AI-credit quota, either because
    /// the entitlement is unlimited or because the plan does not meter it.
    pub premium_interactions: Option<QuotaUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaUsage {
    pub used: u32,
    pub entitlement: u32,
    /// Usage beyond the entitlement, billed per request.
    pub overage_count: u32,
    pub overage_permitted: bool,
}

impl CopilotUsage {
    /// The reset date without any time component, for display.
    pub fn reset_date(&self) -> Option<&str> {
        let resets_on = self.resets_on.as_deref()?;
        Some(resets_on.split('T').next().unwrap_or(resets_on))
    }
}

#[derive(Deserialize)]
struct UserResponse {
    #[serde(default)]
    copilot_plan: Option<String>,
    #[serde(default)]
    quota_reset_date: Option<String>,
    #[serde(default)]
    quota_snapshots: HashMap<String, QuotaSnapshot>,
}

/// Counts are floats on the wire: GitHub charges fractional credits for some
/// models, so a remaining balance of `262.5` is possible.
#[derive(Deserialize)]
struct QuotaSnapshot {
    #[serde(default)]
    unlimited: bool,
    #[serde(default)]
    entitlement: Option<f64>,
    #[serde(default)]
    remaining: Option<f64>,
    #[serde(default)]
    overage_count: Option<f64>,
    #[serde(default)]
    overage_permitted: bool,
}

impl QuotaSnapshot {
    fn into_usage(self) -> Option<QuotaUsage> {
        if self.unlimited {
            return None;
        }

        let entitlement = self.entitlement?;
        if entitlement <= 0.0 || entitlement == UNLIMITED_ENTITLEMENT {
            return None;
        }

        // `remaining` can exceed the entitlement or go negative around overage,
        // so clamp before converting.
        let remaining = self
            .remaining
            .unwrap_or(entitlement)
            .clamp(0.0, entitlement);

        Some(QuotaUsage {
            used: (entitlement - remaining).round() as u32,
            entitlement: entitlement.round() as u32,
            overage_count: self.overage_count.unwrap_or(0.0).max(0.0).round() as u32,
            overage_permitted: self.overage_permitted,
        })
    }
}

pub(crate) async fn request_usage(
    client: &Arc<dyn HttpClient>,
    configuration: &CopilotChatConfiguration,
    oauth_token: &str,
) -> Result<CopilotUsage> {
    let url = configuration.usage_url();
    let editor_version = format!(
        "Zed/{}",
        option_env!("CARGO_PKG_VERSION").unwrap_or("unknown")
    );

    // This is a GitHub REST endpoint rather than one of the
    // `api.githubcopilot.com` chat endpoints, so it deliberately does not reuse
    // `copilot_request_headers`: it wants the `token` authorization scheme and
    // editor identification, and none of the chat-completion headers.
    let request = HttpRequest::builder()
        .method(Method::GET)
        .uri(&url)
        .header("Authorization", format!("token {oauth_token}"))
        .header("Accept", "application/json")
        .header("Editor-Version", editor_version.as_str())
        .header("Editor-Plugin-Version", editor_version.as_str())
        .header("User-Agent", editor_version.as_str())
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .body(AsyncBody::empty())?;

    let mut response = client.send(request).await?;
    let status = response.status();

    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    // Include the body: this endpoint is undocumented enough that the status
    // alone is rarely actionable, and GitHub explains authorization and
    // entitlement problems there.
    anyhow::ensure!(
        status.is_success(),
        "GET {url} failed: {status}: {}",
        truncate_for_error(&body)
    );

    parse_usage(&body)
}

fn truncate_for_error(body: &[u8]) -> String {
    const MAX_LEN: usize = 500;
    let body = String::from_utf8_lossy(body);
    let body = body.trim();
    match body.char_indices().nth(MAX_LEN) {
        Some((index, _)) => format!("{}…", &body[..index]),
        None => body.to_string(),
    }
}

fn parse_usage(body: &[u8]) -> Result<CopilotUsage> {
    let mut parsed: UserResponse =
        serde_json::from_slice(body).context("Failed to parse Copilot usage response")?;

    let snapshot = parsed.quota_snapshots.remove(PREMIUM_INTERACTIONS_QUOTA_ID);
    if snapshot.is_none() {
        // The indicator has nothing to show in this case, so record why rather
        // than silently staying hidden.
        log::info!(
            "GitHub Copilot usage response contains no `{PREMIUM_INTERACTIONS_QUOTA_ID}` quota; \
             plan: {:?}, other quotas: {:?}",
            parsed.copilot_plan,
            parsed.quota_snapshots.keys().collect::<Vec<_>>(),
        );
    }
    let premium_interactions = snapshot.and_then(QuotaSnapshot::into_usage);

    Ok(CopilotUsage {
        plan: parsed.copilot_plan,
        resets_on: parsed.quota_reset_date,
        premium_interactions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_metered_plan() {
        let usage = parse_usage(
            br#"{
                "copilot_plan": "individual",
                "quota_reset_date": "2025-02-15T00:00:00Z",
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 300,
                        "remaining": 240,
                        "percent_remaining": 80,
                        "overage_count": 0,
                        "overage_permitted": false,
                        "unlimited": false
                    },
                    "chat": { "entitlement": 1000, "remaining": 950, "unlimited": false }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(usage.plan.as_deref(), Some("individual"));
        assert_eq!(usage.reset_date(), Some("2025-02-15"));
        assert_eq!(
            usage.premium_interactions,
            Some(QuotaUsage {
                used: 60,
                entitlement: 300,
                overage_count: 0,
                overage_permitted: false,
            })
        );
    }

    #[test]
    fn test_parse_rounds_fractional_credits() {
        let usage = parse_usage(
            br#"{
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 300,
                        "remaining": 262.5,
                        "overage_count": 12.5,
                        "overage_permitted": true
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            usage.premium_interactions,
            Some(QuotaUsage {
                used: 38,
                entitlement: 300,
                overage_count: 13,
                overage_permitted: true,
            })
        );
    }

    #[test]
    fn test_parse_unlimited_and_free_plans() {
        // Explicit `unlimited` flag.
        let usage = parse_usage(
            br#"{"quota_snapshots": {"premium_interactions": {"unlimited": true, "entitlement": 0}}}"#,
        )
        .unwrap();
        assert_eq!(usage.premium_interactions, None);

        // Unlimited signalled via a negative entitlement.
        let usage = parse_usage(
            br#"{"quota_snapshots": {"premium_interactions": {"entitlement": -1, "remaining": -1}}}"#,
        )
        .unwrap();
        assert_eq!(usage.premium_interactions, None);

        // Free plans report `limited_user_quotas` and no premium bucket at all.
        let usage = parse_usage(
            br#"{"copilot_plan": "free", "limited_user_quotas": {"chat": 410}, "quota_snapshots": {}}"#,
        )
        .unwrap();
        assert_eq!(usage.plan.as_deref(), Some("free"));
        assert_eq!(usage.premium_interactions, None);
    }

    #[test]
    fn test_parse_clamps_negative_remaining_during_overage() {
        let usage = parse_usage(
            br#"{"quota_snapshots": {"premium_interactions": {"entitlement": 300, "remaining": -20, "overage_count": 20}}}"#,
        )
        .unwrap();

        let premium_interactions = usage.premium_interactions.unwrap();
        assert_eq!(premium_interactions.used, 300);
        assert_eq!(premium_interactions.overage_count, 20);
    }
}
