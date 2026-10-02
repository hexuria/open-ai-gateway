//! The model catalog, its prices, and registered services.

use crate::Db;
use crate::rows::{ModelRow, ServiceRow, StoredModelRow};
use oag_core::{Error, Result};
use rust_decimal::Decimal;
use std::collections::HashSet;
use uuid::Uuid;

/// The whole model catalog.
pub async fn catalog(db: &Db) -> Result<Vec<ModelRow>> {
    sqlx::query_as::<_, ModelRow>(
        r"
        SELECT id, provider, upstream_name, input_per_mtok, output_per_mtok,
               cache_read_per_mtok, cache_write_per_mtok, context_window,
               max_output_tokens, supports_vision, supports_tools,
               supports_reasoning, supports_prompt_cache, display_label
        FROM model_catalog
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading catalog: {e}")))
}

/// Name a model, or hand it back to the derived default.
///
/// `None` clears the column, which is not the same as writing the derived
/// string into it: a cleared row keeps following the provider's spelling, while
/// a stored copy of today's derivation would go stale the moment the catalog is
/// refreshed.
///
/// No `is_override` guard here, unlike every other write to this table. That
/// flag protects an operator's numbers from an automated refresh, and this *is*
/// the operator — refusing their rename because they had once edited a price
/// would be the guard firing at the person it exists for.
///
/// Returns the id when a row was renamed, `None` when there is no such model,
/// which is the caller's 404.
pub async fn set_model_label(db: &Db, id: &str, label: Option<&str>) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>(
        "UPDATE model_catalog SET display_label = $2, updated_at = now() \
         WHERE id = $1 RETURNING id",
    )
    .bind(id)
    .bind(label)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("labelling model: {e}")))
}

const LIST_SERVICES_SQL: &str = concat!(
    "SELECT ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at ",
    "FROM service ORDER BY name"
);
const SERVICE_BY_ID_SQL: &str = concat!(
    "SELECT ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at ",
    "FROM service WHERE id = $1"
);
const INSERT_SERVICE_SQL: &str = concat!(
    "INSERT INTO service (",
    "id, name, kind, base_url, health_path, dashboard_url, auth_ref",
    ") VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at"
);
const UPDATE_SERVICE_SQL: &str = concat!(
    "UPDATE service SET ",
    "name = $2, kind = $3, base_url = $4, health_path = $5, ",
    "dashboard_url = $6, auth_ref = $7, enabled = $8 ",
    "WHERE id = $1 RETURNING ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at"
);
const RECORD_HEALTH_SQL: &str = concat!(
    "UPDATE service SET ",
    "last_ok = CASE WHEN $2 THEN now() ELSE last_ok END, ",
    "last_error = $3 ",
    "WHERE id = $1 RETURNING ",
    "id, name, kind, base_url, health_path, dashboard_url, ",
    "auth_ref, enabled, last_ok, last_error, created_at"
);

/// Values to insert a catalog row. Validation of URLs and kind belongs to
/// the caller — the store persists what it is given, and the SQL CHECKs are
/// the second line.
#[derive(Debug, Clone)]
pub struct NewService<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub kind: &'a str,
    pub base_url: &'a str,
    pub health_path: &'a str,
    pub dashboard_url: Option<&'a str>,
    pub auth_ref: Option<Uuid>,
}

/// Replacement values for a catalog row. Health columns are not here: they
/// are written only by [`record_service_health`].
#[derive(Debug, Clone)]
pub struct ServiceUpdate<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub base_url: &'a str,
    pub health_path: &'a str,
    pub dashboard_url: Option<&'a str>,
    pub auth_ref: Option<Uuid>,
    pub enabled: bool,
}

pub async fn list_services(db: &Db) -> Result<Vec<ServiceRow>> {
    sqlx::query_as::<_, ServiceRow>(LIST_SERVICES_SQL)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("listing services: {e}")))
}

pub async fn service_by_id(db: &Db, id: Uuid) -> Result<Option<ServiceRow>> {
    sqlx::query_as::<_, ServiceRow>(SERVICE_BY_ID_SQL)
        .bind(id)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("loading service: {e}")))
}

pub async fn insert_service(db: &Db, s: &NewService<'_>) -> Result<ServiceRow> {
    sqlx::query_as::<_, ServiceRow>(INSERT_SERVICE_SQL)
        .bind(s.id)
        .bind(s.name)
        .bind(s.kind)
        .bind(s.base_url)
        .bind(s.health_path)
        .bind(s.dashboard_url)
        .bind(s.auth_ref)
        .fetch_one(db.pool())
        .await
        .map_err(|e| map_service_write_error("creating service", &e))
}

pub async fn update_service(
    db: &Db,
    id: Uuid,
    s: &ServiceUpdate<'_>,
) -> Result<Option<ServiceRow>> {
    sqlx::query_as::<_, ServiceRow>(UPDATE_SERVICE_SQL)
        .bind(id)
        .bind(s.name)
        .bind(s.kind)
        .bind(s.base_url)
        .bind(s.health_path)
        .bind(s.dashboard_url)
        .bind(s.auth_ref)
        .bind(s.enabled)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| map_service_write_error("updating service", &e))
}

/// Take a service out of the catalog's active set, or put it back.
pub async fn set_service_enabled(db: &Db, id: Uuid, enabled: bool) -> Result<Option<String>> {
    sqlx::query_scalar::<_, String>("UPDATE service SET enabled = $2 WHERE id = $1 RETURNING name")
        .bind(id)
        .bind(enabled)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("setting service enabled: {e}")))
}

/// Record the outcome of a health probe.
///
/// A success stamps `last_ok` and clears `last_error`. A failure writes the
/// error and leaves `last_ok` alone, so "was healthy, now is not" stays
/// visible.
pub async fn record_service_health(
    db: &Db,
    id: Uuid,
    ok: bool,
    error: Option<&str>,
) -> Result<Option<ServiceRow>> {
    sqlx::query_as::<_, ServiceRow>(RECORD_HEALTH_SQL)
        .bind(id)
        .bind(ok)
        .bind(error)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("recording service health: {e}")))
}

fn map_service_write_error(what: &str, e: &sqlx::Error) -> Error {
    if let Some(db) = e.as_database_error() {
        match db.code().as_deref() {
            Some("23505") => {
                return Error::Config("a service with that name already exists".to_owned());
            }
            Some("23503") => {
                return Error::Config(
                    "auth_ref does not match a credential in the pool".to_owned(),
                );
            }
            Some("23514") => {
                return Error::Config("service row failed a database check".to_owned());
            }
            _ => {}
        }
    }
    Error::Internal(format!("{what}: {e}"))
}

/// The upsert, as a named constant so a test can read what the conflict branch
/// does and does not touch. The columns it leaves out are the point of it.
pub(super) const UPSERT_MODEL_SQL: &str = r"
        INSERT INTO model_catalog (
            id, provider, upstream_name, input_per_mtok, output_per_mtok,
            cache_read_per_mtok, cache_write_per_mtok, context_window, max_output_tokens,
            supports_vision, supports_tools, supports_reasoning, supports_prompt_cache,
            is_override, display_label
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
        ON CONFLICT (id) DO UPDATE SET
            provider = EXCLUDED.provider,
            upstream_name = EXCLUDED.upstream_name,
            input_per_mtok = EXCLUDED.input_per_mtok,
            output_per_mtok = EXCLUDED.output_per_mtok,
            cache_read_per_mtok = EXCLUDED.cache_read_per_mtok,
            cache_write_per_mtok = EXCLUDED.cache_write_per_mtok,
            context_window = EXCLUDED.context_window,
            max_output_tokens = EXCLUDED.max_output_tokens,
            supports_vision = EXCLUDED.supports_vision,
            supports_tools = EXCLUDED.supports_tools,
            supports_reasoning = EXCLUDED.supports_reasoning,
            supports_prompt_cache = EXCLUDED.supports_prompt_cache,
            updated_at = now()
        -- `display_label` is missing from that list on purpose, exactly as
        -- `is_override` is: a seed carries no label and would write NULL over
        -- whatever the operator called the model. The name is theirs, so only
        -- `set_model_label` writes it, and a re-seed leaves it where it was.
        --
        -- An operator who edited a price meant it. A catalog refresh from
        -- upstream pricing data must not silently undo that.
        WHERE model_catalog.is_override = false
        ";

/// Insert or update a catalog entry, never clobbering an operator override.
pub async fn upsert_model(db: &Db, m: &ModelRow, is_override: bool) -> Result<()> {
    sqlx::query(UPSERT_MODEL_SQL)
        .bind(&m.id)
        .bind(&m.provider)
        .bind(&m.upstream_name)
        .bind(m.input_per_mtok)
        .bind(m.output_per_mtok)
        .bind(m.cache_read_per_mtok)
        .bind(m.cache_write_per_mtok)
        .bind(m.context_window)
        .bind(m.max_output_tokens)
        .bind(m.supports_vision)
        .bind(m.supports_tools)
        .bind(m.supports_reasoning)
        .bind(m.supports_prompt_cache)
        .bind(is_override)
        // Only ever reaches an INSERT: a seed builds rows with no label, and the
        // conflict branch above does not name the column.
        .bind(m.display_label.as_deref())
        .execute(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("upserting model: {e}")))?;
    Ok(())
}

/// The write an operator's own catalog row goes through, as a named constant
/// so a test can read that its conflict branch sets the flag and has no guard.
pub(super) const OVERRIDE_MODEL_SQL: &str = r"
        INSERT INTO model_catalog (
            id, provider, upstream_name, input_per_mtok, output_per_mtok,
            cache_read_per_mtok, cache_write_per_mtok, context_window, max_output_tokens,
            supports_vision, supports_tools, supports_reasoning, supports_prompt_cache,
            is_override, display_label
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,true,$14)
        ON CONFLICT (id) DO UPDATE SET
            provider = EXCLUDED.provider,
            upstream_name = EXCLUDED.upstream_name,
            input_per_mtok = EXCLUDED.input_per_mtok,
            output_per_mtok = EXCLUDED.output_per_mtok,
            cache_read_per_mtok = EXCLUDED.cache_read_per_mtok,
            cache_write_per_mtok = EXCLUDED.cache_write_per_mtok,
            context_window = EXCLUDED.context_window,
            max_output_tokens = EXCLUDED.max_output_tokens,
            supports_vision = EXCLUDED.supports_vision,
            supports_tools = EXCLUDED.supports_tools,
            supports_reasoning = EXCLUDED.supports_reasoning,
            supports_prompt_cache = EXCLUDED.supports_prompt_cache,
            is_override = true,
            display_label = COALESCE(EXCLUDED.display_label, model_catalog.display_label),
            synced_by = NULL,
            updated_at = now()
        ";

/// Write a catalog entry exactly as an operator states it, and mark it theirs.
///
/// Not [`upsert_model`] with `is_override` set. That write protects an
/// operator's row from a refresh: its conflict branch skips a row already
/// overridden and never sets the flag on one that was not. Through it, an
/// operator correcting their own row would be skipped as somebody's override,
/// and a seeded row they repriced would stay a seeded row for the next
/// `catalog seed` to write back over. Here the operator is the writer that
/// guard exists for, so there is no guard: every column is replaced, the flag
/// is set, and a label they already gave survives unless they give another.
///
/// It also clears `synced_by` (migration 0022), so a row a sync wrote and
/// the operator then stated is theirs from then on, and no sync removes it.
pub async fn override_model(db: &Db, m: &ModelRow) -> Result<()> {
    sqlx::query(OVERRIDE_MODEL_SQL)
        .bind(&m.id)
        .bind(&m.provider)
        .bind(&m.upstream_name)
        .bind(m.input_per_mtok)
        .bind(m.output_per_mtok)
        .bind(m.cache_read_per_mtok)
        .bind(m.cache_write_per_mtok)
        .bind(m.context_window)
        .bind(m.max_output_tokens)
        .bind(m.supports_vision)
        .bind(m.supports_tools)
        .bind(m.supports_reasoning)
        .bind(m.supports_prompt_cache)
        .bind(m.display_label.as_deref())
        .execute(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("writing the operator's model: {e}")))?;
    Ok(())
}

/// Refresh only the prices of an existing catalog entry.
///
/// Deliberately not `upsert_model` with a rebuilt row. A provider's own price
/// API is authoritative about money and silent about context windows, so an
/// upsert would carry whatever the caller guessed into `context_window` and
/// `max_output_tokens` — and a window that shrinks from 500k to a guess is a
/// router that quietly stops offering the one model a long request fits in.
/// The columns not named here keep whatever a LiteLLM seed or an operator put
/// there.
///
/// A `None` cache price means the provider did not state one, which is not the
/// same as stating zero, so the existing value survives.
///
/// Returns false when nothing was updated: either no such id, or an operator
/// override, which is left alone for the same reason `upsert_model` leaves it.
pub async fn update_model_prices(
    db: &Db,
    id: &str,
    input_per_mtok: Decimal,
    output_per_mtok: Decimal,
    cache_read_per_mtok: Option<Decimal>,
) -> Result<bool> {
    let done = sqlx::query(
        r"
        UPDATE model_catalog SET
            input_per_mtok = $2,
            output_per_mtok = $3,
            cache_read_per_mtok = COALESCE($4, cache_read_per_mtok),
            updated_at = now()
        WHERE id = $1 AND is_override = false
        ",
    )
    .bind(id)
    .bind(input_per_mtok)
    .bind(output_per_mtok)
    .bind(cache_read_per_mtok)
    .execute(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("repricing model: {e}")))?;

    Ok(done.rows_affected() > 0)
}

/// One provider's catalog rows, and any row of another provider whose id
/// carries this provider's prefix (`<provider>/…`), each with whether an
/// operator override protects it and which endpoint's sync wrote it: what an
/// endpoint's catalog sync compares its list against. The second kind holds
/// an id the sync would write, and is how it learns not to.
pub async fn provider_models(db: &Db, provider: &str) -> Result<Vec<StoredModelRow>> {
    sqlx::query_as::<_, StoredModelRow>(
        r"
        SELECT id, provider, upstream_name, input_per_mtok, output_per_mtok,
               cache_read_per_mtok, cache_write_per_mtok, context_window,
               max_output_tokens, supports_vision, supports_tools,
               supports_reasoning, supports_prompt_cache, display_label, is_override,
               synced_by
        FROM model_catalog
        WHERE provider = $1 OR left(id, char_length($1) + 1) = $1 || '/'
        ORDER BY id
        ",
    )
    .bind(provider)
    .fetch_all(db.pool())
    .await
    .map_err(|e| Error::Internal(format!("loading {provider}'s catalog rows: {e}")))
}

/// Every model id a route's ladder names, read from each rung's `models` as the
/// router reads them. A ladder, or a rung, that is not the shape the router
/// reads names nothing here, and a route's being inactive does not stop its
/// ladder naming a model: it can be switched back on.
///
/// A macro so that the two statements that read it are each one literal.
macro_rules! laddered_models_sql {
    () => {
        "SELECT m.model FROM route r \
         CROSS JOIN LATERAL jsonb_array_elements( \
             CASE jsonb_typeof(r.tiers) WHEN 'array' THEN r.tiers ELSE '[]'::jsonb END \
         ) AS rung(value) \
         CROSS JOIN LATERAL jsonb_array_elements_text( \
             CASE jsonb_typeof(rung.value -> 'models') \
                  WHEN 'array' THEN rung.value -> 'models' ELSE '[]'::jsonb END \
         ) AS m(model)"
    };
}

const LADDERED_MODELS_SQL: &str = laddered_models_sql!();

/// The ids some route's ladder names, as `laddered_models_sql!` reads them.
pub async fn laddered_models(db: &Db) -> Result<HashSet<String>> {
    let ids: Vec<Option<String>> = sqlx::query_scalar(LADDERED_MODELS_SQL)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("reading the ladders: {e}")))?;
    // A JSON null in a rung reads as SQL NULL, and names no model.
    Ok(ids.into_iter().flatten().collect())
}

/// An endpoint sync's write: every column but `provider` from the listing, as
/// an override. Unlike [`UPSERT_MODEL_SQL`], its conflict branch rewrites an
/// override: the rows under an endpoint's name that its list prices are the
/// list's, and a sync that could not update what it wrote last time could not
/// follow a price change.
///
/// It rewrites only a row of the same provider, so an id another provider's
/// row already holds is left as it was and returns nothing. It writes a label
/// only where the row has none: a label is the operator's, as it is to
/// [`UPSERT_MODEL_SQL`], and a sync supplies one only for a row nobody has
/// named.
///
/// Every row it writes is marked as the endpoint's sync's (`synced_by`,
/// migration 0022), which is what lets a later sync remove it, and nothing
/// else lets one: see [`DELETE_STALE_SQL`].
const SYNC_MODEL_SQL: &str = r"
        INSERT INTO model_catalog (
            id, provider, upstream_name, input_per_mtok, output_per_mtok,
            cache_read_per_mtok, cache_write_per_mtok, context_window, max_output_tokens,
            supports_vision, supports_tools, supports_reasoning, supports_prompt_cache,
            is_override, display_label, synced_by
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,true,$14,$2)
        ON CONFLICT (id) DO UPDATE SET
            upstream_name = EXCLUDED.upstream_name,
            input_per_mtok = EXCLUDED.input_per_mtok,
            output_per_mtok = EXCLUDED.output_per_mtok,
            cache_read_per_mtok = EXCLUDED.cache_read_per_mtok,
            cache_write_per_mtok = EXCLUDED.cache_write_per_mtok,
            context_window = EXCLUDED.context_window,
            max_output_tokens = EXCLUDED.max_output_tokens,
            supports_vision = EXCLUDED.supports_vision,
            supports_tools = EXCLUDED.supports_tools,
            supports_reasoning = EXCLUDED.supports_reasoning,
            supports_prompt_cache = EXCLUDED.supports_prompt_cache,
            is_override = true,
            display_label = COALESCE(model_catalog.display_label, EXCLUDED.display_label),
            synced_by = EXCLUDED.synced_by,
            updated_at = now()
        WHERE model_catalog.provider = EXCLUDED.provider
        RETURNING id
        ";

/// Removes the provider's rows named in `$2` that its own sync wrote and no
/// ladder names, and says which it removed.
///
/// Both conditions are read in the statement that removes the row: a row the
/// operator stated with `catalog add` after the caller planned, which clears
/// `synced_by`, and a ladder written after it, each keep the row.
const DELETE_STALE_SQL: &str = concat!(
    "DELETE FROM model_catalog c WHERE c.provider = $1 AND c.synced_by = $1 ",
    "AND c.id = ANY($2) ",
    "AND NOT EXISTS (SELECT 1 FROM (",
    laddered_models_sql!(),
    ") AS laddered WHERE laddered.model = c.id) ",
    "RETURNING c.id"
);

/// What [`sync_endpoint_models`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointSync {
    /// Inserted or rewritten.
    pub written: Vec<String>,
    /// Held by a row of another provider, and left as it was.
    pub held: Vec<String>,
    /// Removed: every id in `stale` that no ladder named. The rest a ladder
    /// names, and they stay.
    pub removed: Vec<String>,
}

/// Write one endpoint's synced rows and remove its stale ones, in one
/// transaction, so a catalog refresh sees the endpoint's rows as they were or
/// as the sync left them and never half of each.
///
/// Every row must be `provider`'s, and is marked as its sync's. A stale row is
/// removed only if an earlier sync of `provider` wrote it and no ladder names
/// it, decided in the statement that removes it: a ladder written, or a row
/// stated by an operator, between the caller's look and this one still keeps
/// its model.
pub async fn sync_endpoint_models(
    db: &Db,
    provider: &str,
    rows: &[ModelRow],
    stale: &[String],
) -> Result<EndpointSync> {
    if let Some(stray) = rows.iter().find(|m| m.provider != provider) {
        return Err(Error::Internal(format!(
            "syncing {provider}'s catalog rows was handed {}, which is {}'s",
            stray.id, stray.provider
        )));
    }
    let mut tx = db
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Internal(format!("starting {provider}'s catalog sync: {e}")))?;
    let mut done = EndpointSync::default();
    for m in rows {
        let written: Option<String> = sqlx::query_scalar(SYNC_MODEL_SQL)
            .bind(&m.id)
            .bind(&m.provider)
            .bind(&m.upstream_name)
            .bind(m.input_per_mtok)
            .bind(m.output_per_mtok)
            .bind(m.cache_read_per_mtok)
            .bind(m.cache_write_per_mtok)
            .bind(m.context_window)
            .bind(m.max_output_tokens)
            .bind(m.supports_vision)
            .bind(m.supports_tools)
            .bind(m.supports_reasoning)
            .bind(m.supports_prompt_cache)
            .bind(m.display_label.as_deref())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| Error::Internal(format!("syncing model {}: {e}", m.id)))?;
        match written {
            Some(id) => done.written.push(id),
            None => done.held.push(m.id.clone()),
        }
    }
    done.removed = sqlx::query_scalar(DELETE_STALE_SQL)
        .bind(provider)
        .bind(stale)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| Error::Internal(format!("removing {provider}'s stale models: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| Error::Internal(format!("committing {provider}'s catalog sync: {e}")))?;
    Ok(done)
}
