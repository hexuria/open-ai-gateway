//! `oag admin key`: minting, listing and revoking inbound keys.

use super::principals::require_admin_principal;
use super::{KeyAction, KeyCli};
use oag_core::Result;
use oag_store::{Db, repo};
use uuid::Uuid;

pub(super) async fn key_cmd(db: &Db, redis_url: &str, cli: KeyCli) -> Result<()> {
    match cli.action {
        Some(KeyAction::Create {
            email,
            route,
            name,
            floor_tier,
            admin,
        }) => {
            // The admin gate wants BOTH the key's flag and the principal's
            // role, so an admin key on a member principal is refused by every
            // admin endpoint it is presented to. It is enforced inside
            // `mint_key`, which is the only place all three callers pass
            // through — this arm and the one below used to check for
            // themselves, and `init` did not check at all.
            let key = mint_key(db, &email, &route, &name, floor_tier.as_deref(), admin).await?;
            print_key(&key);
            Ok(())
        }
        Some(KeyAction::List) => list_keys(db).await,
        Some(KeyAction::Revoke { prefix }) => revoke_key(db, redis_url, &prefix).await,
        None => {
            let Some(email) = cli.email else {
                return Err(oag_core::Error::Config(
                    "oag admin key needs a subcommand; mint one with `oag admin key create --email <email>`"
                        .to_owned(),
                ));
            };
            let key = mint_key(
                db,
                &email,
                cli.route.as_deref().unwrap_or("default"),
                cli.name.as_deref().unwrap_or("cli"),
                cli.floor_tier.as_deref(),
                cli.admin,
            )
            .await?;
            print_key(&key);
            Ok(())
        }
    }
}

async fn list_keys(db: &Db) -> Result<()> {
    let rows: Vec<(String, String, bool, bool, String, String)> = sqlx::query_as(
        r"
        SELECT k.key_prefix, k.name, k.admin, k.active, p.email, r.name
        FROM api_key k
        JOIN principal p ON p.id = k.principal_id
        JOIN route r ON r.id = k.route_id
        ORDER BY k.created_at
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing keys: {e}")))?;

    if rows.is_empty() {
        println!("no keys; mint one with `oag admin key create --email <email>`");
        return Ok(());
    }
    println!("PREFIX             NAME         ADMIN    ACTIVE   EMAIL                    ROUTE");
    for (prefix, name, admin, active, email, route) in rows {
        println!(
            "{prefix:<18} {name:<12} {:<8} {:<8} {email:<24} {route}",
            if admin { "yes" } else { "no" },
            if active { "yes" } else { "no" },
        );
    }
    Ok(())
}

/// Mint a key. The plaintext is returned once and never stored.
pub(super) async fn mint_key(
    db: &Db,
    email: &str,
    route: &str,
    name: &str,
    floor_tier: Option<&str>,
    admin: bool,
) -> Result<String> {
    use std::fmt::Write as _;

    // The admin gate lives here, not at the call sites.
    //
    // C7 put it on the two `key create` arms and missed the third caller.
    // `init` mints with `admin = true` unconditionally and then prints "This is
    // an ADMIN key" — and once C6 stopped `init` promoting an existing
    // principal, that became exactly the key C7 refuses one command over: it
    // authenticates, and every admin endpoint then refuses it. The operator is
    // told they hold admin authority they do not have.
    //
    // This function already takes `admin` and already resolves the principal,
    // so it is the one place a fourth caller cannot forget. `require_admin_
    // principal` passes when there is no principal at all, leaving the missing
    // -row diagnosis below to name both lookups it could have been.
    if admin {
        require_admin_principal(db, email).await?;
    }

    // 32 bytes of entropy. The prefix is there so a leaked key is recognisable
    // in a log and can be grepped for during an incident.
    let mut raw = [0u8; 32];
    // The thread-local CSPRNG, seeded from the OS.
    rand::fill(&mut raw);
    let key = format!(
        "{}{}",
        oag_store::repo::KEY_PREFIX,
        raw.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    );

    let hash = repo::hash_key(&key);
    let prefix: String = key.chars().take(16).collect();

    // `RETURNING id` and `fetch_optional`, not `execute`.
    //
    // The SELECT yields no rows when either lookup misses, so this INSERT
    // inserts nothing — and `execute` reports that as a perfectly successful
    // statement affecting zero rows. The plaintext was then printed with "This
    // is shown once", which was true in the worst possible way: it had never
    // been stored, so it could not be recovered and could never authenticate.
    //
    // The developer holding it gets 401 on every request, `oag admin key list`
    // shows nothing, and the incident reads as broken auth rather than as a
    // mistyped route name. The HTTP twin has always returned `Option` and said
    // which lookup failed; this is the same answer.
    let created: Option<Uuid> = sqlx::query_scalar(
        r"
        INSERT INTO api_key
            (id, key_hash, key_prefix, name, principal_id, route_id, floor_tier, admin)
        SELECT $1, $2, $3, $4, p.id, r.id, $7, $8
        FROM principal p, route r
        WHERE p.email = $5 AND r.name = $6
        RETURNING id
        ",
    )
    .bind(Uuid::now_v7())
    .bind(&hash)
    .bind(&prefix)
    .bind(name)
    .bind(email)
    .bind(route)
    .bind(floor_tier)
    .bind(admin)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("minting key: {e}")))?;

    if created.is_none() {
        // Both lookups named, because the row that is missing is the whole
        // diagnosis and the caller cannot see which of the two it was.
        return Err(oag_core::Error::Config(format!(
            "no key was created: there is no principal with email {email}, or no route \
             named {route}. `oag admin route show --route {route}` says whether the route \
             exists; a principal is created by `oag admin init --email {email}`."
        )));
    }

    Ok(key)
}

pub(super) fn print_key(key: &str) {
    println!("\n  {key}\n");
    println!("  This is shown once. Only its SHA-256 is stored, so it cannot be recovered.");
}

pub(super) async fn revoke_key(db: &Db, redis_url: &str, prefix: &str) -> Result<()> {
    for line in revoke_key_lines(db, redis_url, prefix).await? {
        println!("{line}");
    }
    Ok(())
}

/// What `oag admin key revoke` says, and what it had to reach to say it.
///
/// Returns the lines rather than printing them, so a test can read the one
/// sentence that matters. Which of the two closing paragraphs comes back is the
/// whole of finding C9 and is invisible from outside the process: during a
/// leaked-key incident the operator acts on it, and the two outcomes are
/// fifteen seconds and five minutes of a key that still works.
pub(super) async fn revoke_key_lines(
    db: &Db,
    redis_url: &str,
    prefix: &str,
) -> Result<Vec<String>> {
    let revoked = repo::revoke_key_by_prefix(db, prefix).await?;
    if revoked.is_empty() {
        return Ok(vec![format!("no active key with prefix {prefix}")]);
    }
    let mut lines = Vec::new();

    // Every one of them. `key_prefix` has no unique index, so this UPDATE has
    // always been capable of matching several rows; taking the first and
    // dropping the rest left the others deactivated in the database but still
    // authenticating from the shared cache for its full TTL — and left the
    // operator believing one key had been dealt with.
    let cache = oag_store::Cache::connect(redis_url)?;
    let mut evicted = true;
    for (hash, name, prefix) in &revoked {
        // The row update alone is not a revocation: every replica caches auth
        // by hash, so without this the key keeps working until those entries
        // expire.
        if let Err(e) = cache.auth_invalidate(hash).await {
            evicted = false;
            tracing::warn!(error = %e, %prefix, "the shared cache was not evicted");
        }

        // Same target and shape as the server's audit line, so the CLI is not a
        // hole in the trail — and one line per key, because a collision that
        // revoked someone else's key is exactly what the trail is for.
        tracing::warn!(
            target: "oag::audit",
            actor = "cli",
            action = "key.revoke",
            subject = %prefix,
            name,
            "admin write"
        );
        lines.push(format!("revoked {name} ({prefix})"));
    }

    // Said loudly, because it means a key nobody asked about has just stopped
    // working. The prefix is displayed and not unique, so this is reachable
    // without anything being wrong with the database.
    if revoked.len() > 1 {
        lines.push(format!(
            "\n  NOTE: {} keys shared the prefix {prefix} and all of them were revoked.",
            revoked.len()
        ));
        lines.push(
            "  If you meant only one, the others are named above and need re-issuing.".to_owned(),
        );
    }
    // Said only when it happened. `auth_invalidate` used to swallow both an
    // unreachable Redis and a failed DEL, so this line printed either way — and
    // during a leaked-key incident it is the sentence the operator acts on. The
    // difference between the two outcomes is fifteen seconds and five minutes.
    if evicted {
        lines.push(
            "  shared cache evicted; each replica's in-process cache expires within 15s".to_owned(),
        );
    } else {
        lines.push(String::new());
        lines.push("  WARNING: the shared cache was NOT evicted — see the log above.".to_owned());
        lines.push("  The key is inactive in the database but every replica will keep".to_owned());
        lines.push("  accepting it from the cache for up to 5 minutes. Retry with".to_owned());
        lines.push("  `oag admin cache flush` once the cache is reachable.".to_owned());
    }
    Ok(lines)
}
