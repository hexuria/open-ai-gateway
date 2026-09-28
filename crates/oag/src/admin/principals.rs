//! `oag admin principal`: the identities keys are minted against, and their roles.

use oag_core::Result;
use oag_store::{Db, repo};
use rust_decimal::Decimal;
use uuid::Uuid;

/// Refuse to mint an admin key for a principal who is not an admin.
///
/// The gate is an AND of two facts and a key can only carry one of them. A key
/// minted with `--admin` against a member principal authenticates fine and is
/// refused by every admin endpoint, which reads as the admin API being broken
/// rather than as the key being half-privileged.
pub(super) async fn require_admin_principal(db: &Db, email: &str) -> Result<()> {
    match principal_role(db, email).await?.as_deref() {
        // An admin principal, or no principal at all — the missing case is left
        // to `mint_key`, which names both lookups it could have been.
        Some("admin") | None => Ok(()),
        Some(role) => Err(oag_core::Error::Config(format!(
            "{email} is a {role}, so an --admin key minted for them would authenticate \
             and then be refused by every admin endpoint: the gate needs an admin key AND \
             an admin principal. Grant the role first with \
             `oag admin principal promote --email {email}`, or drop --admin for an \
             inference key."
        ))),
    }
}

/// The role this principal holds, or `None` if there is no such principal.
pub(super) async fn principal_role(db: &Db, email: &str) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>("SELECT role FROM principal WHERE email = $1")
        .bind(email)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("reading principal role: {e}")))
}

/// Grant the admin role. The one place a role changes.
///
/// Separate from `init` because granting authority should be the whole of what
/// a command does, not a consequence of asking it to add a route — see
/// [`upsert_principal`]. Idempotent: promoting an admin is a no-op that says so.
///
/// There is deliberately no `demote`. The admin gate wants both an admin key and
/// an admin principal, so removing the role from the last admin locks every
/// human out of the admin API with no way back in through it — and the CLI is
/// reached by whoever holds the database, which is a different and larger
/// permission. A role that needs removing can be removed there, deliberately,
/// by someone who has just had to think about it.
pub(super) async fn promote_principal(db: &Db, email: &str) -> Result<()> {
    let Some(role) = principal_role(db, email).await? else {
        return Err(oag_core::Error::Config(format!(
            "no principal with email {email}. `oag admin init --email {email}` creates one."
        )));
    };
    if role == "admin" {
        println!("{email} is already an admin");
        return Ok(());
    }
    sqlx::query("UPDATE principal SET role = 'admin', updated_at = now() WHERE email = $1")
        .bind(email)
        .execute(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("promoting principal: {e}")))?;

    // Same target and shape as every other admin write, because granting
    // authority is the one an auditor most wants to find.
    tracing::warn!(
        target: "oag::audit",
        actor = "cli",
        action = "principal.promote",
        subject = %email,
        from = %role,
        "admin write"
    );
    println!("{email} promoted from {role} to admin");
    println!("  Existing keys are unaffected; an admin key still needs `--admin`.");
    Ok(())
}

/// Drop every cached identity belonging to `principal`'s keys.
///
/// A budget lives in the cached auth context, not only in the row, so lowering
/// a cap without evicting leaves it unenforced for the cache's full five
/// minutes — on every replica, with nothing in the CLI's output hinting that a
/// flush is needed. The HTTP path for the same write has always evicted
/// explicitly; this is the same call from the other surface.
///
/// Best-effort, and warns rather than fails: the write has already happened,
/// and a principal whose keys could not be evicted is worth saying so about,
/// not worth failing a command that succeeded.
pub(super) async fn evict_principal_keys(db: &Db, redis_url: &str, principal: Uuid, email: &str) {
    let hashes = match repo::key_hashes_for_principal(db, principal).await {
        Ok(hashes) => hashes,
        Err(e) => {
            tracing::warn!(error = %e, %email, "could not list this principal's keys to evict");
            return;
        }
    };
    if hashes.is_empty() {
        return;
    }
    let cache = match oag_store::Cache::connect(redis_url) {
        Ok(cache) => cache,
        Err(e) => {
            tracing::warn!(error = %e, %email, "could not reach the cache to evict");
            println!("  NOTE: the new budget is not enforced until the auth cache expires (5m).");
            return;
        }
    };
    let mut failed = 0usize;
    for hash in &hashes {
        if cache.auth_invalidate(hash).await.is_err() {
            failed += 1;
        }
    }
    if failed > 0 {
        println!(
            "  NOTE: {failed} of {} cached identities could not be evicted; the new budget \
             is not enforced for them until the cache expires (5m).",
            hashes.len()
        );
    }
}

/// Create a principal, or update the budget of one that exists.
///
/// **The role is written on insert and never on conflict.** `init` asks for
/// `admin`, which is right for the principal it is creating — promoting the
/// first admin is what the command is for — and wrong for one that already
/// exists. `ON CONFLICT ... SET role = EXCLUDED.role` meant that adding a
/// second route with
/// `oag admin init --email someone@corp.com --route staging` silently granted
/// admin to whoever that email named and then minted them an admin key. Nothing
/// in the output said a role had changed, because from the command's point of
/// view nothing had: it had asked for an admin and been given one.
///
/// The store's own `upsert_principal` has always omitted `role` here and says
/// why at length — an idempotent bind must not be able to change authority. The
/// same argument applies in this direction; only the sign is different. Granting
/// a role is now `oag admin principal promote`, where it is the whole of the
/// caller's stated intent rather than a side effect of adding a route.
///
/// The budget is still `COALESCE`d rather than overwritten, so an `init` that
/// omits `--budget-usd` cannot erase one an operator set.
pub(super) async fn upsert_principal(
    db: &Db,
    email: &str,
    role: &str,
    budget: Option<Decimal>,
) -> Result<Uuid> {
    let id: (Uuid,) = sqlx::query_as(
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
    .map_err(|e| oag_core::Error::Internal(format!("creating principal: {e}")))?;
    Ok(id.0)
}
