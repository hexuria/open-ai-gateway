//! Why a provider is absent from `/v1/models` `data`.
//!
//! The picker hides a spent, reserved, rate-limited, or disabled seat, and it
//! empties entirely when the caller is out of money. That filtering is right.
//! This module is the read-only explanation: the same seats, including the ones
//! that cannot serve, aggregated per provider so one call fills the picker
//! *and* the status panel.
//!
//! Operator account names (`grok-seat`, `mock`) stay off the wire. They made
//! junk credentials obvious to a human reading Postgres, but they are operator
//! labels; provider + reason + numbers cover the cases a caller can act on.

use oag_core::Provider;
use oag_core::credential::CredentialKind;
use oag_router::BudgetPressure;
use oag_store::ChannelStatusRow;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;

/// Closed set of reasons a provider is or is not serving. Never free text,
/// never a secret, never an upstream error string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PresenceReason {
    /// At least one scoped credential would pass [`oag_store::repo::route_channels`].
    Serving,
    /// Remaining allowance is above zero but at or below the operator reserve.
    Reserved,
    /// Provider `Retry-After` is still in the future.
    RateLimited,
    /// Remaining allowance is zero (or below), reserve or not.
    QuotaSpent,
    /// Operator took it out of rotation.
    Disabled,
    /// The route names this provider but this principal holds no credential.
    NoCredential,
    /// The caller cannot spend; seats themselves may be healthy.
    BudgetExhausted,
}

impl PresenceReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Serving => "serving",
            Self::Reserved => "reserved",
            Self::RateLimited => "rate_limited",
            Self::QuotaSpent => "quota_spent",
            Self::Disabled => "disabled",
            Self::NoCredential => "no_credential",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// One provider as the listing envelope reports it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderPresence {
    pub provider: Provider,
    pub serving: bool,
    pub reason: PresenceReason,
    pub until: Option<OffsetDateTime>,
    pub remaining_pct: Option<Decimal>,
    pub reserve_pct: Option<i16>,
    pub kinds: Vec<String>,
    pub models: usize,
}

impl ProviderPresence {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "provider": self.provider.as_str(),
            "serving": self.serving,
            "reason": self.reason.as_str(),
            "until": self.until.and_then(rfc3339),
            "remaining_pct": pct_json(self.remaining_pct),
            "reserve_pct": self.reserve_pct.map(f64::from),
            "kinds": self.kinds,
            "models": self.models,
        })
    }
}

/// Aggregate the route's scoped credentials into one row per provider.
///
/// Several seats for one provider collapse: if any would serve, the provider
/// serves, unless the caller is out of money. If none would, the reason is the
/// most recoverable of the seats — reserved before rate-limited before spent
/// before disabled — so a status panel names the thing that will move first.
pub(crate) fn diagnose(
    rows: &[ChannelStatusRow],
    pressure: BudgetPressure,
    model_counts: &BTreeMap<Provider, usize>,
    now: OffsetDateTime,
) -> Vec<ProviderPresence> {
    let mut grouped: BTreeMap<Provider, Vec<&ChannelStatusRow>> = BTreeMap::new();
    for row in rows {
        let Ok(provider) = row.provider.parse::<Provider>() else {
            tracing::warn!(
                provider = %row.provider,
                "account.provider is not a known provider; not listing it in diagnostics"
            );
            continue;
        };
        grouped.entry(provider).or_default().push(row);
    }
    grouped
        .into_iter()
        .map(|(provider, group)| summarise(provider, &group, pressure, model_counts, now))
        .collect()
}

fn summarise(
    provider: Provider,
    rows: &[&ChannelStatusRow],
    pressure: BudgetPressure,
    model_counts: &BTreeMap<Provider, usize>,
    now: OffsetDateTime,
) -> ProviderPresence {
    let mut kinds = BTreeSet::new();
    let mut best_serving: Option<Facts> = None;
    let mut best_blocked: Option<Facts> = None;

    for row in rows {
        if let Some(kind) = kind_label(&row.kind) {
            kinds.insert(kind.to_owned());
        }
        let reason = classify(row, now);
        let facts = Facts {
            reason,
            until: until_for(reason, row, now),
            remaining_pct: row.usage_remaining_pct,
            reserve_pct: row.usage_reserve_pct,
        };
        if reason == PresenceReason::Serving {
            best_serving = Some(better_serving(best_serving, facts));
        } else {
            best_blocked = Some(more_recoverable(best_blocked, facts));
        }
    }

    let (mut serving, mut facts) = match best_serving {
        Some(facts) => (true, facts),
        None => (
            false,
            best_blocked.unwrap_or(Facts {
                reason: PresenceReason::NoCredential,
                until: None,
                remaining_pct: None,
                reserve_pct: None,
            }),
        ),
    };

    // Out of money is a caller fact, not a seat fact. A healthy seat still
    // cannot serve this key, so it must not read as `serving` — but a seat
    // that is itself reserved or spent keeps that reason, because waiting
    // for Thursday is a different sentence from "raise the quota".
    if pressure == BudgetPressure::Exhausted {
        serving = false;
        if facts.reason == PresenceReason::Serving {
            facts.reason = PresenceReason::BudgetExhausted;
            facts.until = None;
        }
    }

    let models = if serving {
        model_counts.get(&provider).copied().unwrap_or(0)
    } else {
        0
    };

    ProviderPresence {
        provider,
        serving,
        reason: facts.reason,
        until: facts.until,
        remaining_pct: facts.remaining_pct,
        reserve_pct: facts.reserve_pct,
        kinds: kinds.into_iter().collect(),
        models,
    }
}

#[derive(Clone, Copy)]
struct Facts {
    reason: PresenceReason,
    until: Option<OffsetDateTime>,
    remaining_pct: Option<Decimal>,
    reserve_pct: Option<i16>,
}

/// Same predicate [`oag_store::repo::route_channels`] uses, plus the reason
/// when it fails. Disabled first (operator choice), then the provider's own
/// backoff, then spent vs reserved.
fn classify(row: &ChannelStatusRow, now: OffsetDateTime) -> PresenceReason {
    if !row.schedulable {
        return PresenceReason::Disabled;
    }
    if row.rate_limited_until.is_some_and(|t| t > now) {
        return PresenceReason::RateLimited;
    }
    match row.usage_remaining_pct {
        Some(left) if left <= Decimal::ZERO => PresenceReason::QuotaSpent,
        Some(left) if left <= Decimal::from(row.usage_reserve_pct.unwrap_or(0)) => {
            PresenceReason::Reserved
        }
        _ => PresenceReason::Serving,
    }
}

fn until_for(
    reason: PresenceReason,
    row: &ChannelStatusRow,
    now: OffsetDateTime,
) -> Option<OffsetDateTime> {
    let t = match reason {
        PresenceReason::RateLimited => row.rate_limited_until,
        PresenceReason::Reserved | PresenceReason::QuotaSpent => row.window_resets_at,
        _ => None,
    }?;
    (t > now).then_some(t)
}

fn kind_label(column: &str) -> Option<&'static str> {
    match CredentialKind::from_column(column)? {
        CredentialKind::ApiKey => Some("api"),
        CredentialKind::OAuth => Some("sub"),
        CredentialKind::Bedrock => Some("bedrock"),
        CredentialKind::Vertex => Some("vertex"),
        CredentialKind::ServiceAccount => Some("service_account"),
        // Closed for the kinds this build knows. A new kind still has a
        // column spelling; the qualifier vocabulary does not grow until
        // someone names it here.
        _ => None,
    }
}

fn better_serving(current: Option<Facts>, candidate: Facts) -> Facts {
    let Some(current) = current else {
        return candidate;
    };
    if serving_rank(candidate.remaining_pct) > serving_rank(current.remaining_pct) {
        candidate
    } else {
        current
    }
}

fn serving_rank(remaining: Option<Decimal>) -> Decimal {
    remaining.unwrap_or(Decimal::ONE_HUNDRED)
}

fn more_recoverable(current: Option<Facts>, candidate: Facts) -> Facts {
    let Some(current) = current else {
        return candidate;
    };
    if candidate.reason < current.reason {
        candidate
    } else {
        current
    }
}

fn rfc3339(t: OffsetDateTime) -> Option<String> {
    t.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

fn pct_json(d: Option<Decimal>) -> Value {
    d.and_then(|d| d.to_f64())
        .and_then(serde_json::Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-08-29 12:00:00 UTC);
    const THURSDAY: OffsetDateTime = datetime!(2026-09-13 00:00:00 UTC);

    fn row(provider: &str, kind: &str) -> ChannelStatusRow {
        ChannelStatusRow {
            provider: provider.to_owned(),
            kind: kind.to_owned(),
            schedulable: true,
            rate_limited_until: None,
            window_resets_at: None,
            usage_remaining_pct: None,
            usage_reserve_pct: None,
        }
    }

    fn diagnose_one(
        rows: &[ChannelStatusRow],
        pressure: BudgetPressure,
        models: usize,
    ) -> ProviderPresence {
        let mut counts = BTreeMap::new();
        if models > 0 {
            counts.insert(Provider::XAI, models);
            counts.insert(Provider::OpenAI, models);
            counts.insert(Provider::Anthropic, models);
        }
        let got = diagnose(rows, pressure, &counts, NOW);
        assert_eq!(got.len(), 1, "{got:?}");
        got.into_iter().next().unwrap()
    }

    #[test]
    fn a_reserved_seat_is_present_and_explains_itself() {
        // grok-seat: remaining 8 against reserve 15. The picker hides xAI;
        // the envelope must still say reserved, with the numbers.
        let mut grok = row("xai", "oauth");
        grok.usage_remaining_pct = Some(dec!(8));
        grok.usage_reserve_pct = Some(15);

        let got = diagnose_one(&[grok], BudgetPressure::Normal, 4);
        assert!(!got.serving);
        assert_eq!(got.reason, PresenceReason::Reserved);
        assert_eq!(got.remaining_pct, Some(dec!(8)));
        assert_eq!(got.reserve_pct, Some(15));
        assert_eq!(got.kinds, ["sub"]);
        assert_eq!(got.models, 0);
        assert_eq!(got.until, None);

        let json = got.to_json();
        assert_eq!(json["reason"], "reserved");
        assert_eq!(json["remaining_pct"], 8.0);
        assert_eq!(json["reserve_pct"], 15.0);
        assert!(!json.to_string().contains("grok-seat"));
        assert!(!json.to_string().contains("mock"));
        assert!(json.get("name").is_none());
    }

    #[test]
    fn a_rate_limited_seat_names_when_it_returns() {
        // codex-seat: rate_limited_until 2026-09-13. Same empty picker, a
        // date instead of a remaining-pct.
        let mut codex = row("openai", "oauth");
        codex.rate_limited_until = Some(THURSDAY);

        let got = diagnose_one(&[codex], BudgetPressure::Normal, 3);
        assert!(!got.serving);
        assert_eq!(got.reason, PresenceReason::RateLimited);
        assert_eq!(got.until, Some(THURSDAY));
        assert_eq!(got.kinds, ["sub"]);
        assert_eq!(got.models, 0);
        assert_eq!(got.to_json()["until"], "2026-09-13T00:00:00Z");
    }

    #[test]
    fn a_spent_seat_is_quota_spent_even_without_a_reserve() {
        let mut seat = row("xai", "oauth");
        seat.usage_remaining_pct = Some(dec!(0));

        let got = diagnose_one(&[seat], BudgetPressure::Normal, 4);
        assert!(!got.serving);
        assert_eq!(got.reason, PresenceReason::QuotaSpent);
        assert_eq!(got.remaining_pct, Some(dec!(0)));
        assert_eq!(got.models, 0);
    }

    #[test]
    fn remaining_at_the_reserve_line_is_reserved_not_spent() {
        let mut seat = row("xai", "oauth");
        seat.usage_remaining_pct = Some(dec!(15));
        seat.usage_reserve_pct = Some(15);

        let got = diagnose_one(&[seat], BudgetPressure::Normal, 1);
        assert_eq!(got.reason, PresenceReason::Reserved);
        assert!(!got.serving);
    }

    #[test]
    fn a_zero_remaining_seat_with_a_reserve_is_spent_not_reserved() {
        // Nothing left is spent, even if a reserve is also set. Reserved is
        // the band *above* zero and at or below the line.
        let mut seat = row("xai", "oauth");
        seat.usage_remaining_pct = Some(Decimal::ZERO);
        seat.usage_reserve_pct = Some(15);

        let got = diagnose_one(&[seat], BudgetPressure::Normal, 1);
        assert_eq!(got.reason, PresenceReason::QuotaSpent);
    }

    #[test]
    fn a_disabled_seat_says_disabled() {
        let mut seat = row("anthropic", "api_key");
        seat.schedulable = false;

        let got = diagnose_one(&[seat], BudgetPressure::Normal, 28);
        assert_eq!(got.reason, PresenceReason::Disabled);
        assert!(!got.serving);
        assert_eq!(got.kinds, ["api"]);
        assert_eq!(got.models, 0);
    }

    #[test]
    fn a_live_seat_serves_and_keeps_its_model_count() {
        // The mock-anthropic afternoon: three junk keys serving 28 models.
        // Without the account name the client still sees serving anthropic,
        // 28 models, kind api — not "no models".
        let live = row("anthropic", "api_key");
        let got = diagnose_one(&[live], BudgetPressure::Normal, 28);
        assert!(got.serving);
        assert_eq!(got.reason, PresenceReason::Serving);
        assert_eq!(got.models, 28);
        assert_eq!(got.kinds, ["api"]);
        assert_eq!(got.remaining_pct, None);
    }

    #[test]
    fn budget_exhausted_and_spent_seats_both_empty_data_but_disagree() {
        // The distinction the whole change exists for. Both empty the picker;
        // only the envelope says whether to wait for a window or raise a cap.
        let live = row("xai", "oauth");
        let mut spent = row("xai", "oauth");
        spent.usage_remaining_pct = Some(Decimal::ZERO);

        let out_of_money = diagnose_one(&[live], BudgetPressure::Exhausted, 4);
        let seats_spent = diagnose_one(&[spent], BudgetPressure::Normal, 4);

        assert!(!out_of_money.serving);
        assert!(!seats_spent.serving);
        assert_eq!(out_of_money.models, 0);
        assert_eq!(seats_spent.models, 0);
        assert_eq!(out_of_money.reason, PresenceReason::BudgetExhausted);
        assert_eq!(seats_spent.reason, PresenceReason::QuotaSpent);
        assert_ne!(out_of_money.reason, seats_spent.reason);
        assert_eq!(out_of_money.to_json()["reason"], "budget_exhausted");
        assert_eq!(seats_spent.to_json()["reason"], "quota_spent");
    }

    #[test]
    fn an_exhausted_budget_does_not_hide_a_reserved_reason() {
        // The seat itself will not serve on Thursday either. Keep reserved
        // so a status panel does not pretend raising the quota is enough.
        let mut grok = row("xai", "oauth");
        grok.usage_remaining_pct = Some(dec!(8));
        grok.usage_reserve_pct = Some(15);

        let got = diagnose_one(&[grok], BudgetPressure::Exhausted, 4);
        assert_eq!(got.reason, PresenceReason::Reserved);
        assert!(!got.serving);
    }

    #[test]
    fn a_provider_with_one_live_seat_still_serves() {
        let live = row("xai", "api_key");
        let mut spent = row("xai", "oauth");
        spent.usage_remaining_pct = Some(Decimal::ZERO);

        let got = diagnose(
            &[live, spent],
            BudgetPressure::Normal,
            &BTreeMap::from([(Provider::XAI, 2)]),
            NOW,
        );
        assert_eq!(got.len(), 1);
        assert!(got[0].serving);
        assert_eq!(got[0].reason, PresenceReason::Serving);
        assert_eq!(got[0].kinds, ["api", "sub"]);
        assert_eq!(got[0].models, 2);
    }

    #[test]
    fn unknown_remaining_is_serving_not_empty() {
        // A provider with no usage API must not vanish, and must not report
        // a remaining_pct it does not have.
        let key = row("anthropic", "api_key");
        let got = diagnose_one(&[key], BudgetPressure::Normal, 1);
        assert_eq!(got.reason, PresenceReason::Serving);
        assert_eq!(got.remaining_pct, None);
        assert_eq!(got.to_json()["remaining_pct"], Value::Null);
    }

    #[test]
    fn an_unparseable_provider_is_dropped_not_listed() {
        let junk = row("not-a-provider", "api_key");
        let got = diagnose(&[junk], BudgetPressure::Normal, &BTreeMap::new(), NOW);
        assert!(got.is_empty());
    }

    #[test]
    fn no_credential_is_a_closed_enum_member() {
        // Listed so a client match is exhaustive. Emitted when a provider
        // group has no classifiable seat, which the query does not produce
        // on its own.
        assert_eq!(PresenceReason::NoCredential.as_str(), "no_credential");
    }
}
