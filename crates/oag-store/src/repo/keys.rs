//! Inbound keys and their principals: hashing, authentication, minting, revocation.

use crate::Db;
use crate::rows::AuthContext;
use oag_core::{Error, Result};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

/// Hash an inbound key for lookup.
///
/// The key is never stored in the clear, so this is also the only way to find
/// one. Plaintext keys matched on column equality would turn read access to
/// one table into every client's credential.
/// What every key this gateway has ever issued looks like: the prefix, then
/// 32 bytes of entropy as lowercase hex. See `mint_key`.
pub const KEY_PREFIX: &str = "oag_live_";
/// The stored, loggable prefix of an issued key: `KEY_PREFIX` plus seven hex.
///
/// Sixteen characters, the same slice `mint_key` records in `api_key.key_prefix`,
/// so a prefix in a log lines up with a prefix in the database and an operator
/// can join the two without ever handling a value.
///
/// **Only ever call this on a string that has passed [`is_issued_key_shape`].**
/// The point of the prefix is that it identifies one of *our* keys; the first
/// sixteen characters of an arbitrary bearer token are the first sixteen
/// characters of somebody else's secret, and a caller who pastes an Anthropic
/// key into the wrong client must not have a third of it written to a log.
#[must_use]
pub fn loggable_key_prefix(raw: &str) -> String {
    raw.chars().take(16).collect()
}

/// `KEY_PREFIX` plus 64 hex digits.
pub const KEY_LEN: usize = 9 + 64;

/// Whether `raw` has the shape of a key `mint_key` could have produced.
///
/// A syntactic check, and the *only* thing that runs before the database on
/// the inference surface. Every issued key has exactly this shape, so a
/// string without it is not an unknown key — it is not a key — and refusing
/// it here costs nothing. Without this, a flood of arbitrary strings in the
/// `Authorization` header bought a Redis GET and a Postgres probe apiece from
/// anyone who could reach the port, ahead of any rate limit: the one lookup
/// on the request path that no valid credential was needed to trigger.
///
/// What this is not: a negative cache. A random key per request misses a
/// negative cache exactly as it misses the positive one, and a fleet-wide
/// "this key does not exist" entry would lock a freshly minted key out for
/// its TTL. Shape is the cheap filter; a permit around the lookup itself
/// (`AuthCache`) is the bound on what gets past it.
#[must_use]
pub fn is_issued_key_shape(raw: &str) -> bool {
    raw.len() == KEY_LEN
        && raw.starts_with(KEY_PREFIX)
        && raw[KEY_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[must_use]
pub fn hash_key(raw: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Look up an inbound key and everything the request path needs from it.
///
/// One query rather than three. Auth is on the hot path of every request, and
/// the difference between one round trip and three is the difference between a
/// cache miss costing 1ms and 3ms.
pub async fn authenticate(db: &Db, raw_key: &str) -> Result<Option<AuthContext>> {
    let hash = hash_key(raw_key);
    let now = OffsetDateTime::now_utc();

    // Identity and limits only. Spend is not here, on purpose: this row is
    // what the auth cache holds for minutes, and a spend figure cached for
    // minutes is a cap that N concurrent requests all pass together. Spend is
    // read fresh by `spend_for`, per request, from the columns `record_usage`
    // maintains. (This query used to SUM the principal's month from the
    // ledger on every cache miss, and then cache the answer — the worst of
    // both: a scan, and stale.)
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            Uuid,
            Option<String>,
            Option<Decimal>,
            Option<Decimal>,
            Decimal,
            bool,
            Option<OffsetDateTime>,
        ),
    >(
        r"
        SELECT k.id, k.principal_id, k.route_id, k.floor_tier,
               k.quota_usd,
               p.monthly_budget_usd, p.hard_stop_multiple,
               k.admin, k.expires_at
        FROM api_key k
        JOIN principal p ON p.id = k.principal_id
        JOIN route    r ON r.id = k.route_id
        WHERE k.key_hash = $1
          AND k.active AND p.active AND r.active
          AND (k.expires_at IS NULL OR k.expires_at > $2)
        ",
    )
    .bind(&hash)
    .bind(now)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("authenticating: {e}")))?;

    Ok(row.map(|r| AuthContext {
        api_key_id: r.0,
        principal_id: r.1,
        route_id: r.2,
        key_floor_tier: r.3,
        quota_usd: r.4,
        principal_budget_usd: r.5,
        principal_hard_stop_multiple: r.6,
        admin: r.7,
        expires_at: r.8,
    }))
}

/// Revoke an inbound key. Returns `(key_hash, name, key_prefix)`.
///
/// The hash comes back because the caller must evict the auth cache and never
/// holds the plaintext. It must not be put in a response body.
pub async fn revoke_key(db: &Db, id: Uuid) -> Result<Option<(String, String, String)>> {
    sqlx::query_as::<_, (String, String, String)>(
        "UPDATE api_key SET active = false WHERE id = $1 RETURNING key_hash, name, key_prefix",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("revoking key: {e}")))
}

/// A freshly minted key. `key` is the plaintext, and this is the only time it
/// exists anywhere — it is hashed on the way into the row.
#[derive(Debug, Clone)]
pub struct MintedKey {
    pub id: Uuid,
    pub prefix: String,
    pub key: String,
}

/// Create or update a principal, returning its id.
///
/// The identity-integration surface: a partner service (`OpenGrok`) binds each of
/// its orgs to one principal, so the org's budget and usage rollup are this
/// row's. Idempotent on `email`, which is the only stable handle a caller that
/// stores no gateway ids can use.
///
/// `budget` is `COALESCE`d rather than overwritten so an upsert-before-mint
/// cannot silently erase a budget an operator set at the CLI; clearing one is
/// [`set_principal_budget`]'s job, where it is the caller's stated intent.
///
/// [`set_principal_budget`]: super::set_principal_budget
///
/// **`role` IS NOT UPDATED ON CONFLICT, and that is the point.** This path can only
/// ever ask for `member`, so updating the role would mean an upsert against an
/// existing admin's email SILENTLY DEMOTES them — and since the admin gate wants
/// both an admin key and an admin principal, that locks a human operator out of
/// the admin API without touching their key. An idempotent bind must not be able
/// to remove authority. Granting or changing a role stays the CLI's job (the CLI
/// keeps its own upsert, which does write the role, because promoting the first
/// admin is exactly what `oag admin init` is for).
pub async fn upsert_principal(
    db: &Db,
    email: &str,
    role: &str,
    budget: Option<Decimal>,
) -> Result<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        r"
        INSERT INTO principal (id, email, role, monthly_budget_usd)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (email) DO UPDATE SET
            monthly_budget_usd = COALESCE(EXCLUDED.monthly_budget_usd, principal.monthly_budget_usd),
            updated_at = now()
        RETURNING id
        ",
    )
    .bind(Uuid::now_v7())
    .bind(email)
    .bind(role)
    .bind(budget)
    .fetch_one(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("upserting principal: {e}")))
}

/// Mint an inbound key on an existing principal and route. Returns the plaintext
/// ONCE — it is hashed on the way in and is not recoverable afterwards.
///
/// `None` means the principal or route does not exist, so a caller naming either
/// wrongly is told rather than silently given nothing.
///
/// `quota_usd` is the per-key spend cap (the per-member cap in the identity
/// integration); `None` leaves the key uncapped and bounded only by the
/// principal's monthly budget.
pub async fn mint_key(
    db: &Db,
    principal_email: &str,
    route: &str,
    name: &str,
    quota_usd: Option<Decimal>,
) -> Result<Option<MintedKey>> {
    use std::fmt::Write as _;

    // 32 bytes of entropy. The prefix exists so a leaked key is recognisable in
    // a log and greppable during an incident.
    let mut raw = [0u8; 32];
    // The thread-local CSPRNG, seeded from the OS.
    rand::fill(&mut raw);
    let key = format!(
        "{KEY_PREFIX}{}",
        raw.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    );
    let hash = hash_key(&key);
    let prefix = loggable_key_prefix(&key);

    // Never `admin`: a key minted over HTTP must not be able to mint more keys.
    // Admin authority is the CLI's to grant (`oag admin key create --admin`).
    let id = sqlx::query_scalar::<_, Uuid>(
        r"
        INSERT INTO api_key
            (id, key_hash, key_prefix, name, principal_id, route_id, quota_usd, admin)
        SELECT $1, $2, $3, $4, p.id, r.id, $7, false
        FROM principal p, route r
        WHERE p.email = $5 AND r.name = $6
        RETURNING id
        ",
    )
    .bind(Uuid::now_v7())
    .bind(&hash)
    .bind(&prefix)
    .bind(name)
    .bind(principal_email)
    .bind(route)
    .bind(quota_usd)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("minting key: {e}")))?;

    Ok(id.map(|id| MintedKey { id, prefix, key }))
}

/// Set (or clear) one key's spend cap. Returns `(name, key_prefix, key_hash)`;
/// `None` means no key with that id.
///
/// The hash is returned so the caller can evict the key's cached identity:
/// the cap lives in `AuthContext`, which every tier holds for minutes, and a
/// lowered cap that nothing invalidates is not enforced until the entry
/// happens to expire — while the 200 the operator got asserted the new value.
pub async fn set_key_quota(
    db: &Db,
    id: Uuid,
    quota_usd: Option<Decimal>,
) -> Result<Option<(String, String, String)>> {
    sqlx::query_as::<_, (String, String, String)>(
        "UPDATE api_key SET quota_usd = $2 WHERE id = $1 RETURNING name, key_prefix, key_hash",
    )
    .bind(id)
    .bind(quota_usd)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("setting key quota: {e}")))
}

/// Every key hash a principal owns, for evicting them all after a write to
/// the principal's own limits — which every one of those keys carries in its
/// cached identity.
pub async fn key_hashes_for_principal(db: &Db, principal_id: Uuid) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>("SELECT key_hash FROM api_key WHERE principal_id = $1")
        .bind(principal_id)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("listing a principal's keys: {e}")))
}

/// Whether an id names a key at all — an empty per-model report needs to know.
pub async fn key_exists(db: &Db, id: Uuid) -> Result<bool> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM api_key WHERE id = $1)")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("looking up a key: {e}")))
}

/// Points spent inside a window by each of several keys — one query, the partner service's
/// pool read (a member's pool is the sum over that member's coworker keys). Keys with no rows
/// are absent; the caller says 0 for them.
/// A principal's id from its email, for the routes that address it that way.
///
/// `None` means no such principal — distinct from a principal with no ledger
/// rows, which is a real zero and must not be reported as a missing one.
pub async fn principal_id_for_email(db: &Db, email: &str) -> Result<Option<Uuid>> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM principal WHERE email = $1")
        .bind(email)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("looking up a principal: {e}")))
}

/// Revoke by the displayed prefix, for the CLI — during an incident the prefix
/// is what an operator can actually see.
///
/// Every match, and every match is returned. `key_prefix` carries no unique
/// index and this UPDATE has no LIMIT, so a collision has always deactivated
/// more than one row; what it did not do was say so. The caller took
/// `fetch_optional`, which keeps the first row and drops the rest — so the
/// other keys were deactivated in the database, their hashes were never evicted
/// from the shared cache, and they went on authenticating from L2 for the five
/// minutes of its TTL. An operator revoking a leaked key during an incident was
/// told one key was revoked, and got one eviction, while some other customer's
/// key had been switched off behind their back and the leaked one might not
/// have stopped.
///
/// Over-revoking on a collision is the right direction for an incident tool —
/// under-revoking is what gets someone breached — but only if it is visible.
/// Returning the whole set is what makes it visible.
pub async fn revoke_key_by_prefix(db: &Db, prefix: &str) -> Result<Vec<(String, String, String)>> {
    sqlx::query_as::<_, (String, String, String)>(
        "UPDATE api_key SET active = false WHERE key_prefix = $1 AND active
         RETURNING key_hash, name, key_prefix",
    )
    .bind(prefix)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("revoking key by prefix: {e}")))
}
