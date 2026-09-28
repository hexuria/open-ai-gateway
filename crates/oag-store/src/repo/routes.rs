//! Routes and the channels (accounts) each one can use.

use crate::Db;
use crate::rows::{AccountRow, ChannelStatusRow, RouteRow};
use oag_core::{Error, Result};
use uuid::Uuid;

pub async fn route_by_id(db: &Db, id: Uuid) -> Result<Option<RouteRow>> {
    sqlx::query_as::<_, RouteRow>(
        // One primary-key read. This used to SUM the route's month from the
        // ledger whenever the route had a budget — on every inference request,
        // every /v1/models call and every count_tokens call, uncached, over a
        // range that grew all month. Setting `monthly_budget_usd`, an ordinary
        // documented control, changed the asymptotic cost of the request path.
        // `record_usage` now maintains the column; the CASE reads it as zero
        // once the month it names has passed.
        "SELECT id, name, tiers, default_mode, floor_tier, rpm_limit, monthly_budget_usd, active,
                CASE WHEN spent_month = date_trunc('month', now())::date
                     THEN spent_usd ELSE 0 END AS spent_usd
         FROM route WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading route: {e}")))
}

/// Credentials a route may draw on for one provider.
///
/// Personal credentials are filtered here rather than in the scheduler: a
/// credential bound to someone else must never appear in another principal's
/// candidate set, and enforcing that in SQL means it cannot be forgotten by a
/// later change to selection policy.
pub async fn candidates(
    db: &Db,
    route_id: Uuid,
    provider: &str,
    principal_id: Uuid,
) -> Result<Vec<AccountRow>> {
    sqlx::query_as::<_, AccountRow>(
        r"
        SELECT a.id, a.name, a.provider, a.kind,
               a.credentials_sealed, a.credentials_nonce, a.token_version, a.token_expires_at,
               a.owner_principal_id, a.proxy_url, a.priority, a.max_concurrency,
               a.schedulable, a.cooldown_until, a.rate_limited_until, a.window_resets_at,
               a.usage_remaining_pct, a.usage_reserve_pct,
               a.last_used_at
        FROM account a
        JOIN account_route ar ON ar.account_id = a.id
        WHERE ar.route_id = $1
          AND a.provider = $2
          AND (a.owner_principal_id IS NULL OR a.owner_principal_id = $3)
        ",
    )
    .bind(route_id)
    .bind(provider)
    .bind(principal_id)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading candidates: {e}")))
}

/// Providers this route holds usable credentials for, and by which credential
/// kind, for one principal.
///
/// The kind rides along rather than being a second query because the listing
/// needs both answers about the same instant: it offers `<model>@sub` only
/// where a subscription is actually reachable, and two queries could disagree
/// about that across a credential being disabled between them.
///
/// Mirrors the personal-credential predicate in `candidates`: a credential
/// bound to another principal must never appear in this principal's view. Adds
/// `a.schedulable`, which `candidates` leaves to the scheduler — correct here
/// because a disabled credential is an operator decision, not a transient
/// state, and advertising a model nobody can reach is worse than omitting it.
///
/// Access-token `token_expires_at` is not a filter: that is the OAuth access
/// token TTL, refreshed on the request path, not the subscription. Hiding on
/// it would empty a picker every time a fifteen-minute token lapsed between
/// polls. Subscription expiry is `usage_remaining_pct`.
pub async fn route_channels(
    db: &Db,
    route_id: Uuid,
    principal_id: Uuid,
) -> Result<Vec<(String, String, Option<Vec<String>>)>> {
    // `served_models` rides along because it is a property of the same
    // credential row and the listing needs both together: which channels exist,
    // and what each one will actually accept. A NULL here is "never asked", and
    // the caller must treat it as unknown rather than as empty.
    sqlx::query_as::<_, (String, String, Option<Vec<String>>)>(
        r"
        SELECT DISTINCT a.provider, a.kind, a.served_models
        FROM account a
        JOIN account_route ar ON ar.account_id = a.id
        WHERE ar.route_id = $1
          AND a.schedulable
          AND (a.owner_principal_id IS NULL OR a.owner_principal_id = $2)
          -- Exhausted for a while, not merely busy. A seat whose weekly pool is
          -- spent cannot serve a request today, so offering its models lists
          -- something that is certain to fail. The breaker's own cooldown is
          -- deliberately NOT consulted: it lasts seconds, while a client can
          -- cache this list for far longer, so hiding a model mid-blip removes
          -- it until the client next refreshes -- worse than briefly offering
          -- one that fails over to another credential anyway.
          AND (a.rate_limited_until IS NULL OR a.rate_limited_until <= now())
          -- A spent subscription cannot serve, even when no reserve was set.
          -- COALESCE(reserve, 0) makes an unset reserve a floor of zero:
          -- remaining 50 lists, remaining 0 does not. NULL remaining stays
          -- listed — unknown is not empty, and a provider with no usage API
          -- must not vanish from the catalogue for want of a reading.
          AND (a.usage_remaining_pct IS NULL
               OR a.usage_remaining_pct > COALESCE(a.usage_reserve_pct, 0))
        ",
    )
    .bind(route_id)
    .bind(principal_id)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading route providers: {e}")))
}

/// Every credential this principal may draw on for this route, including ones
/// that cannot serve right now.
///
/// [`route_channels`] is the picker: it hides a spent, reserved, rate-limited,
/// or disabled seat so `/v1/models` `data` does not advertise a 503. This is
/// the status panel: the same owner predicate, none of those serving filters,
/// so a caller whose picker is empty can still learn *why*. Name and sealed
/// material stay off the SELECT — an inference key is not an inventory dump.
pub async fn route_channel_status(
    db: &Db,
    route_id: Uuid,
    principal_id: Uuid,
) -> Result<Vec<ChannelStatusRow>> {
    sqlx::query_as::<_, ChannelStatusRow>(
        r"
        SELECT a.provider, a.kind, a.schedulable,
               a.rate_limited_until, a.window_resets_at,
               a.usage_remaining_pct, a.usage_reserve_pct
        FROM account a
        JOIN account_route ar ON ar.account_id = a.id
        WHERE ar.route_id = $1
          AND (a.owner_principal_id IS NULL OR a.owner_principal_id = $2)
        ",
    )
    .bind(route_id)
    .bind(principal_id)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading route credential status: {e}")))
}
