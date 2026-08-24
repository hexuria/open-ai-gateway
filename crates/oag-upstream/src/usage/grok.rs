//! xAI/Grok remaining-quota reading.
//!
//! Contract ported from openusage's
//! `Sources/OpenUsage/Providers/Grok/{GrokUsageClient,GrokCreditsConfigDecoder}.swift`
//! (MIT). The billing endpoint returns a proto3 message as JSON, so zero-valued
//! fields are omitted — an absent `creditUsagePercent` is a real 0%, not drift.

use super::UsageSnapshot;
use oag_core::{Error, Result};
use serde::Deserialize;

/// The same endpoint the Grok CLI calls (`billing.rs` appends
/// `/billing?format=credits`); it shares the CLI's stability and auth.
const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";

/// The billing endpoint only returns a weekly pool for unified-billing users;
/// an account still on monthly-only has no weekly percentage to report, and
/// mislabelling a monthly figure as weekly would be worse than a blank.
const WEEKLY_PERIOD: &str = "USAGE_PERIOD_TYPE_WEEKLY";

pub async fn fetch(access_token: &str) -> Result<Option<UsageSnapshot>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Internal(format!("building usage client: {e}")))?;

    let response = client
        .get(BILLING_URL)
        .header("authorization", format!("Bearer {}", access_token.trim()))
        // The CLI-identifying header the billing proxy expects.
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Internal(format!("grok billing request: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::Internal(format!(
            "grok billing returned {}",
            response.status()
        )));
    }

    let body: Response = response
        .json()
        .await
        .map_err(|e| Error::Internal(format!("grok billing body: {e}")))?;
    Ok(parse(body))
}

#[derive(Deserialize)]
struct Response {
    config: Option<Config>,
}

#[derive(Deserialize)]
struct Config {
    /// 0..100, omitted (so `None`) at exactly 0 by proto-JSON.
    #[serde(rename = "creditUsagePercent")]
    used_percent: Option<f64>,
    #[serde(rename = "currentPeriod")]
    current_period: Option<Period>,
}

#[derive(Deserialize)]
struct Period {
    #[serde(rename = "type")]
    period_type: Option<String>,
    end: Option<String>,
}

fn parse(body: Response) -> Option<UsageSnapshot> {
    let config = body.config?;
    let period = config.current_period?;
    // Only a weekly pool is a figure we can label honestly.
    if period.period_type.as_deref() != Some(WEEKLY_PERIOD) {
        return None;
    }
    let used = config.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    Some(UsageSnapshot {
        remaining_pct: 100.0 - used,
        window_label: "weekly".to_owned(),
        resets_at: period.end.as_deref().and_then(parse_rfc3339),
    })
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339)
        .map(time::OffsetDateTime::unix_timestamp)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shape openusage documents as observed live (2026-07-06).
    const WEEKLY: &str = r#"{ "config": {
        "creditUsagePercent": 99.0,
        "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY",
                           "start": "2026-07-03T04:01:09.238389+00:00",
                           "end":   "2026-07-10T04:01:09.238389+00:00" },
        "onDemandCap": { "val": 2500 } } }"#;

    #[test]
    fn a_weekly_pool_yields_remaining_and_reset() {
        let snap = parse(serde_json::from_str(WEEKLY).expect("json")).expect("weekly");
        assert!(
            (snap.remaining_pct - 1.0).abs() < 1e-9,
            "99% used → 1% left"
        );
        assert_eq!(snap.window_label, "weekly");
        assert_eq!(snap.resets_at, Some(1_783_656_069));
    }

    #[test]
    fn an_absent_percent_is_a_real_zero_used() {
        // proto-JSON drops a 0 value; that is 0% used, i.e. full, not missing.
        let body: Response = serde_json::from_str(
            r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-07-10T00:00:00Z"}}}"#,
        )
        .expect("json");
        let snap = parse(body).expect("weekly");
        assert!((snap.remaining_pct - 100.0).abs() < 1e-9);
    }

    #[test]
    fn a_non_weekly_period_is_left_blank_not_mislabelled() {
        let body: Response = serde_json::from_str(
            r#"{"config":{"creditUsagePercent":40,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_MONTHLY","end":"2026-08-01T00:00:00Z"}}}"#,
        )
        .expect("json");
        assert!(parse(body).is_none());
    }

    #[test]
    fn an_empty_body_is_none() {
        assert!(parse(serde_json::from_str("{}").expect("json")).is_none());
    }
}
