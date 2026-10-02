//! Endpoints: upstreams an operator registers, rather than ones built into the
//! code.
//!
//! The store persists what it is given. Parsing the dialect, platform and auth
//! style, and refusing a built-in provider's name, is the caller's job. The
//! schema's CHECKs (migrations 0020 and 0021) are the second line, and a write
//! they refuse comes back as [`Error::Config`] naming the constraint that
//! refused it.

use crate::Db;
use crate::rows::EndpointRow;
use oag_core::{Error, Result};
use std::collections::HashMap;
use time::OffsetDateTime;

const LIST_ENDPOINTS_SQL: &str = concat!(
    "SELECT ",
    "name, dialect, platform, base_url, auth, region, project, api_version, path, ",
    "extra_headers, display_name, discover_models, created_at, updated_at ",
    "FROM endpoint ORDER BY name"
);
const ENDPOINT_BY_NAME_SQL: &str = concat!(
    "SELECT ",
    "name, dialect, platform, base_url, auth, region, project, api_version, path, ",
    "extra_headers, display_name, discover_models, created_at, updated_at ",
    "FROM endpoint WHERE name = $1"
);
const INSERT_ENDPOINT_SQL: &str = concat!(
    "INSERT INTO endpoint (",
    "name, dialect, platform, base_url, auth, region, project, api_version, ",
    "extra_headers, display_name, discover_models, path",
    ") VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) RETURNING ",
    "name, dialect, platform, base_url, auth, region, project, api_version, path, ",
    "extra_headers, display_name, discover_models, created_at, updated_at"
);
// Written only over the row as the writer read it (`updated_at = $11`), and
// always to a later `updated_at` than the one it replaces, even within one
// microsecond of that write or against a clock that stepped back: a stamp
// that could come out equal is one a stale `seen` could match.
const UPDATE_ENDPOINT_SQL: &str = concat!(
    "UPDATE endpoint SET ",
    "base_url = $2, auth = $3, region = $4, project = $5, api_version = $6, ",
    "extra_headers = $7, display_name = $8, discover_models = $9, path = $10, ",
    "updated_at = greatest(now(), updated_at + interval '1 microsecond') ",
    "WHERE name = $1 AND updated_at = $11 RETURNING ",
    "name, dialect, platform, base_url, auth, region, project, api_version, path, ",
    "extra_headers, display_name, discover_models, created_at, updated_at"
);

/// Values to register an endpoint. Validation belongs to the caller.
#[derive(Debug, Clone)]
pub struct NewEndpoint<'a> {
    pub name: &'a str,
    pub dialect: &'a str,
    pub platform: &'a str,
    pub base_url: Option<&'a str>,
    pub auth: &'a str,
    pub region: Option<&'a str>,
    pub project: Option<&'a str>,
    pub api_version: Option<&'a str>,
    /// Where a `system_one` endpoint takes a question set, beneath its base
    /// URL. `None` is `/v1/systemone`; any other dialect must leave it `None`.
    pub path: Option<&'a str>,
    /// A JSON object of headers that carry no authority. Never a key.
    pub extra_headers: &'a serde_json::Value,
    pub display_name: Option<&'a str>,
    pub discover_models: bool,
}

/// Replacement values for an endpoint's settings.
///
/// No `dialect` and no `platform`. They are what the endpoint is: they decide
/// which codec and which signer every credential and model under its name is
/// served through. Changing either one makes it a different endpoint, so the
/// way to do that is to remove this one and register the other.
#[derive(Debug, Clone)]
pub struct EndpointUpdate<'a> {
    pub base_url: Option<&'a str>,
    pub auth: &'a str,
    pub region: Option<&'a str>,
    pub project: Option<&'a str>,
    pub api_version: Option<&'a str>,
    /// A setting, not part of what the endpoint is: a System One host that
    /// moves its path is the same host.
    pub path: Option<&'a str>,
    pub extra_headers: &'a serde_json::Value,
    pub display_name: Option<&'a str>,
    pub discover_models: bool,
    /// The `updated_at` of the row these settings were made from. The update
    /// is written only while the row still has it, so a writer that read the
    /// row before someone else changed it cannot put back what they changed:
    /// it gets [`EndpointUpdated::Changed`] instead.
    pub seen: OffsetDateTime,
}

/// What [`update_endpoint`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum EndpointUpdated {
    /// Written, and this is the row now. Boxed: a whole row beside two
    /// empty variants would make every answer as large.
    Updated(Box<EndpointRow>),
    /// There is no endpoint by that name: the caller's 404.
    NotFound,
    /// Someone wrote the endpoint after the caller read it, so its
    /// `updated_at` is no longer `seen`, and nothing was written: the caller's
    /// 409, or its cue to read the row again.
    Changed,
}

/// What names one endpoint: its credentials, its catalog models, and the places
/// those models hold on a ladder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EndpointReferences {
    /// Credentials filed under the endpoint's name.
    pub accounts: i64,
    /// Of those, the ones in rotation.
    pub schedulable: i64,
    /// Catalog models whose provider is the endpoint.
    pub models: i64,
    /// Of those, the ones some active route's ladder names.
    pub on_ladder: i64,
}

/// Every model id an active route's ladder names, and each endpoint's four
/// counts against it. A route's `tiers` is JSON a hand-written row can shape
/// any way, so anything that is not a list of rungs holding a list of models
/// counts as naming nothing rather than failing the read.
const ENDPOINT_REFERENCES_SQL: &str = r"
    WITH laddered AS (
        SELECT jsonb_array_elements_text(
                   CASE WHEN jsonb_typeof(rung -> 'models') = 'array'
                        THEN rung -> 'models' ELSE '[]'::jsonb END
               ) AS id
        FROM route r,
             jsonb_array_elements(
                 CASE WHEN jsonb_typeof(r.tiers) = 'array'
                      THEN r.tiers ELSE '[]'::jsonb END
             ) AS rung
        WHERE r.active
    )
    SELECT e.name,
           (SELECT count(*) FROM account a WHERE a.provider = e.name),
           (SELECT count(*) FROM account a WHERE a.provider = e.name AND a.schedulable),
           (SELECT count(*) FROM model_catalog m WHERE m.provider = e.name),
           (SELECT count(*) FROM model_catalog m
             WHERE m.provider = e.name AND m.id IN (SELECT id FROM laddered))
    FROM endpoint e
";

/// Each endpoint's [`EndpointReferences`], by name. One read for all of them:
/// a listing shows every endpoint's counts, and there are few endpoints.
pub async fn endpoint_references(db: &Db) -> Result<HashMap<String, EndpointReferences>> {
    let rows: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(ENDPOINT_REFERENCES_SQL)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("counting what names each endpoint: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|(name, accounts, schedulable, models, on_ladder)| {
            (
                name,
                EndpointReferences {
                    accounts,
                    schedulable,
                    models,
                    on_ladder,
                },
            )
        })
        .collect())
}

/// What [`delete_endpoint`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointDeletion {
    Deleted,
    /// There is no endpoint by that name: the caller's 404.
    NotFound,
    /// Refused, and nothing was removed: this many credentials and catalog
    /// models still name the endpoint as their provider. The caller's 409.
    InUse {
        accounts: i64,
        models: i64,
    },
}

pub async fn list_endpoints(db: &Db) -> Result<Vec<EndpointRow>> {
    sqlx::query_as::<_, EndpointRow>(LIST_ENDPOINTS_SQL)
        .fetch_all(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("listing endpoints: {e}")))
}

pub async fn get_endpoint(db: &Db, name: &str) -> Result<Option<EndpointRow>> {
    sqlx::query_as::<_, EndpointRow>(ENDPOINT_BY_NAME_SQL)
        .bind(name)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| Error::Internal(format!("loading endpoint: {e}")))
}

/// Register an endpoint. A name already taken is an [`Error::Config`] that says
/// it "already exists". A row a CHECK refuses is an [`Error::Config`] that names
/// the check.
pub async fn insert_endpoint(db: &Db, e: &NewEndpoint<'_>) -> Result<EndpointRow> {
    sqlx::query_as::<_, EndpointRow>(INSERT_ENDPOINT_SQL)
        .bind(e.name)
        .bind(e.dialect)
        .bind(e.platform)
        .bind(e.base_url)
        .bind(e.auth)
        .bind(e.region)
        .bind(e.project)
        .bind(e.api_version)
        .bind(e.extra_headers)
        .bind(e.display_name)
        .bind(e.discover_models)
        .bind(e.path)
        .fetch_one(db.pool())
        .await
        .map_err(|err| endpoint_write_error("registering endpoint", e.name, &err))
}

/// Replace an endpoint's settings and stamp `updated_at`, if the row is still
/// the one the settings were made from (`e.seen`).
///
/// Optimistic: nothing is locked between the caller's read and this write, so
/// two writers never wait on each other, and the second to write finds the
/// first one's stamp where it expected its own read's. Which of the two
/// arrived first is the database's to say: an update that waits on another's
/// row lock reads the row again once that one commits, and its `seen` no
/// longer matches.
pub async fn update_endpoint(
    db: &Db,
    name: &str,
    e: &EndpointUpdate<'_>,
) -> Result<EndpointUpdated> {
    let updated = sqlx::query_as::<_, EndpointRow>(UPDATE_ENDPOINT_SQL)
        .bind(name)
        .bind(e.base_url)
        .bind(e.auth)
        .bind(e.region)
        .bind(e.project)
        .bind(e.api_version)
        .bind(e.extra_headers)
        .bind(e.display_name)
        .bind(e.discover_models)
        .bind(e.path)
        .bind(e.seen)
        .fetch_optional(db.pool())
        .await
        .map_err(|err| endpoint_write_error("updating endpoint", name, &err))?;
    if let Some(row) = updated {
        return Ok(EndpointUpdated::Updated(Box::new(row)));
    }
    // Nothing matched: either there is no such endpoint, or there is and it
    // has moved on from `seen`.
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM endpoint WHERE name = $1)")
        .bind(name)
        .fetch_one(db.pool())
        .await
        .map_err(|err| Error::Internal(format!("reading endpoint after an update: {err}")))?;
    Ok(if exists {
        EndpointUpdated::Changed
    } else {
        EndpointUpdated::NotFound
    })
}

/// Remove an endpoint, unless a credential or a catalog model still names it.
///
/// Removing one that is still named would strand those rows. A credential
/// would keep a sealed key that nothing can reach, and a model would stay on
/// ladders it can no longer serve from. Neither `account.provider` nor
/// `model_catalog.provider` can carry a foreign key, because both also name
/// built-in providers, which have no endpoint row. So the refusal is made
/// here and comes back as [`EndpointDeletion::InUse`], not as a database
/// error.
///
/// The lock, the count and the delete run in one transaction, and the lock
/// comes first. It is `FOR UPDATE` because that is the mode that conflicts with
/// the `FOR KEY SHARE` which 0020's `endpoint_reference_holds` trigger takes
/// for every write naming the endpoint. (`FOR NO KEY UPDATE` would not
/// conflict with it.) A write already in flight makes this wait, and the
/// count, a fresh statement under READ COMMITTED, then sees what it wrote. A
/// write that arrives after the lock waits instead. If the delete goes ahead,
/// the trigger refuses that write, because the row it named is gone. If the
/// delete is refused, the write goes through.
///
/// Two gaps remain, both documented rather than closed:
///
/// - A hand-written `DELETE FROM endpoint` is not counted. The guard is this
///   function, not the table.
/// - A write that began before the endpoint existed found nothing to lock. If
///   it stays uncommitted while the endpoint is registered and then deleted,
///   the count cannot see it, and it commits afterwards. The row it leaves
///   names a provider nobody has registered. The request path already ignores
///   such a row (`to_candidate` and `to_spec` cannot parse it), so it serves
///   nothing.
pub async fn delete_endpoint(db: &Db, name: &str) -> Result<EndpointDeletion> {
    let mut tx = db
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Internal(format!("starting endpoint delete: {e}")))?;
    let held =
        sqlx::query_scalar::<_, String>("SELECT name FROM endpoint WHERE name = $1 FOR UPDATE")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| Error::Internal(format!("locking endpoint: {e}")))?;
    // Returning early drops `tx`, which rolls it back and releases the lock.
    if held.is_none() {
        return Ok(EndpointDeletion::NotFound);
    }
    let (accounts, models) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT (SELECT count(*) FROM account WHERE provider = $1), \
                (SELECT count(*) FROM model_catalog WHERE provider = $1)",
    )
    .bind(name)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| Error::Internal(format!("counting what names endpoint: {e}")))?;
    if accounts > 0 || models > 0 {
        return Ok(EndpointDeletion::InUse { accounts, models });
    }
    sqlx::query("DELETE FROM endpoint WHERE name = $1")
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Internal(format!("deleting endpoint: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| Error::Internal(format!("committing endpoint delete: {e}")))?;
    Ok(EndpointDeletion::Deleted)
}

/// A write the schema refused is the caller's mistake, so it is a `Config`
/// error, and the message names the constraint because the constraint is the
/// only thing that knows which rule was broken.
fn endpoint_write_error(what: &str, name: &str, e: &sqlx::Error) -> Error {
    if let Some(db) = e.as_database_error() {
        match db.code().as_deref() {
            Some("23505") => {
                return Error::Config(format!("an endpoint named '{name}' already exists"));
            }
            Some("23514") => {
                return Error::Config(format!(
                    "endpoint '{name}' fails the database check {}",
                    db.constraint().unwrap_or("on the endpoint table")
                ));
            }
            _ => {}
        }
    }
    Error::Internal(format!("{what}: {e}"))
}
