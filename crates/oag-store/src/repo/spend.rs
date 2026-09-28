//! Spend against budgets, and the monthly reconciliation that keeps it honest.

use crate::Db;
use crate::rows::Spend;
use oag_core::{Error, Result};
use rust_decimal::Decimal;
use uuid::Uuid;

/// The caller's spend, fresh.
///
/// One primary-key read on each of two rows, never a SUM: `record_usage`
/// maintains `api_key.spent_usd` (lifetime) and `principal.spent_usd` (the
/// month named by `spent_month`) in the same statement as the ledger insert,
/// so this is exactly as current as the ledger is. A month that has rolled
/// over reads as zero until the first write of the new month resets the row.
///
/// `Err(Unauthenticated)` rather than zeros when the key is gone: a key
/// deleted between authentication and here must not spend as if uncapped for
/// the rest of the cache window.
pub async fn spend_for(db: &Db, api_key_id: Uuid, principal_id: Uuid) -> Result<Spend> {
    let row = sqlx::query_as::<_, (Decimal, Decimal)>(
        r"
        SELECT k.spent_usd,
               CASE WHEN p.spent_month = date_trunc('month', now())::date
                    THEN p.spent_usd ELSE 0 END
          FROM api_key k
          JOIN principal p ON p.id = $2
         WHERE k.id = $1
        ",
    )
    .bind(api_key_id)
    .bind(principal_id)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("reading spend: {e}")))?;

    let (key_usd, principal_usd) = row.ok_or(Error::Unauthenticated)?;
    Ok(Spend {
        key_usd,
        principal_usd,
    })
}

/// What one pass of [`reconcile_monthly_spend`] rewrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub principals: u64,
    pub routes: u64,
}

/// Bring every budgeted principal's and route's monthly counter back into
/// agreement with the ledger.
///
/// `record_usage` maintains the counters in the same statement as the ledger
/// insert, so in steady state there is nothing to do. The gap is the
/// rolling-deploy window: the release that introduced the counters backfilled
/// them once, and the previous release keeps writing ledger rows without
/// touching them for as long as its replicas serve — half an hour by default
/// on every platform — so the spend of that window was invisible to the cap
/// for the rest of the month. This closes it, and closes any drift with the
/// same cause after it.
///
/// One transaction per row, and the row is locked BEFORE the sum is taken.
/// The obvious single statement, `UPDATE ... SET spent_usd = (SELECT SUM ...)`,
/// loses a concurrent debit: when it waits on the row lock the debit holds and
/// then re-evaluates, Postgres re-checks the WHERE clause against the new row
/// version but does not re-run the subquery, so the sum predates the debit and
/// overwrites it. With the lock taken first, a concurrent `record_usage` waits
/// on it, and its ledger row and its debit land together after the sum — which
/// then adds to the reconciled value rather than being lost from it.
///
/// Budgeted rows only: they are the only ones enforced, and every other row's
/// counter is a number nothing reads. The month only moves forward, as in the
/// debit: a row already stamped with a later month is left alone.
///
/// Imported ledger rows carry no principal or route, so a sum keyed on either
/// excludes them without asking, exactly as the backfill did.
pub async fn reconcile_monthly_spend(db: &Db) -> Result<Reconciled> {
    let principals = reconcile_rows(
        db,
        "SELECT id FROM principal WHERE monthly_budget_usd IS NOT NULL",
        "SELECT 1 FROM principal WHERE id = $1 FOR UPDATE",
        r"
        UPDATE principal p
           SET spent_usd = COALESCE((
                   SELECT SUM(u.cost_usd) FROM usage_event u
                    WHERE u.principal_id = p.id
                      AND u.occurred_at >= date_trunc('month', now())
               ), 0),
               spent_month = date_trunc('month', now())::date
         WHERE p.id = $1
           AND (p.spent_month IS NULL OR p.spent_month <= date_trunc('month', now())::date)
        ",
    )
    .await?;
    let routes = reconcile_rows(
        db,
        "SELECT id FROM route WHERE monthly_budget_usd IS NOT NULL",
        "SELECT 1 FROM route WHERE id = $1 FOR UPDATE",
        r"
        UPDATE route r
           SET spent_usd = COALESCE((
                   SELECT SUM(u.cost_usd) FROM usage_event u
                    WHERE u.route_id = r.id
                      AND u.occurred_at >= date_trunc('month', now())
               ), 0),
               spent_month = date_trunc('month', now())::date
         WHERE r.id = $1
           AND (r.spent_month IS NULL OR r.spent_month <= date_trunc('month', now())::date)
        ",
    )
    .await?;
    Ok(Reconciled { principals, routes })
}

/// The per-row half of [`reconcile_monthly_spend`]: list, then lock and
/// rewrite each in its own transaction.
///
/// Static SQL, handed in: the two tables differ only in name, and sqlx will
/// not take a table name as a parameter.
async fn reconcile_rows(
    db: &Db,
    list: &'static str,
    lock: &'static str,
    rewrite: &'static str,
) -> Result<u64> {
    let ids = sqlx::query_scalar::<_, Uuid>(list)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("listing budgeted rows: {e}")))?;

    let mut rewritten = 0u64;
    for id in ids {
        let mut tx = db
            .pool()
            .begin()
            .await
            .map_err(|e| Error::Internal(format!("starting reconcile: {e}")))?;
        // A row deleted since the listing is nothing to reconcile.
        let held = sqlx::query_scalar::<_, i32>(lock)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| Error::Internal(format!("locking row for reconcile: {e}")))?;
        if held.is_none() {
            continue;
        }
        let done = sqlx::query(rewrite)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::Internal(format!("reconciling monthly spend: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| Error::Internal(format!("committing reconcile: {e}")))?;
        rewritten += done.rows_affected();
    }
    Ok(rewritten)
}

/// Set (or clear, with `None`) a principal's monthly budget. `None` return means
/// no principal with that email.
pub async fn set_principal_budget(
    db: &Db,
    email: &str,
    budget: Option<Decimal>,
) -> Result<Option<Uuid>> {
    sqlx::query_scalar::<_, Uuid>(
        "UPDATE principal SET monthly_budget_usd = $2, updated_at = now()
         WHERE email = $1 RETURNING id",
    )
    .bind(email)
    .bind(budget)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("setting principal budget: {e}")))
}
