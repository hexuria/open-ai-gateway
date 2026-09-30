//! Reading an endpoint's model list for an operator: what `oag admin endpoint
//! models` prints, and what `oag admin endpoint sync` writes into the catalog.
//!
//! Here rather than in the CLI because none of it is about a terminal. It takes
//! the database and the key-encryption key, reads the list with one of the
//! endpoint's own credentials, and reports what it found and did, so a test can
//! drive all of it against a stand-in upstream and a gateway serving the rows
//! it wrote.
//!
//! A sync writes `<endpoint>/<upstream id>` rows. The upstream id is the list's
//! own name for the model and may hold slashes of its own (Merge's
//! `zai/glm-5.3-flash`): the endpoint's name is everything before the first
//! slash of the catalog id, and the request carries everything after it.

use oag_core::credential::SecretMaterial;
use oag_core::provider::{AuthStyle, Platform};
use oag_core::{Error, Kek, Result};
use oag_store::{Db, ModelRow, StoredModelRow, repo};
use oag_upstream::custom::EndpointSpec;
use oag_upstream::listing::{self, ListedModel, Offer, PriceChoice, Skip};
use std::collections::{HashMap, HashSet};

/// An endpoint, ready to have its list read.
struct Reader {
    name: String,
    /// What its models' labels name it: its display name, or else its name.
    label: String,
    discover: bool,
    spec: EndpointSpec,
    /// The credential the list is read with, by name; `None` for an endpoint
    /// that takes no key and has none.
    account: Option<String>,
    proxy: Option<String>,
    /// Kept whole, so the key is wiped with it.
    material: Option<SecretMaterial>,
}

impl Reader {
    fn key(&self) -> &str {
        self.material
            .as_ref()
            .map_or("", |m| m.access_token.as_str())
    }
}

/// The endpoint named, with the credential named or else its first
/// schedulable one, or why its list cannot be read.
async fn reader(db: &Db, kek: &Kek, endpoint: &str, account: Option<&str>) -> Result<Reader> {
    let row = repo::get_endpoint(db, endpoint)
        .await?
        .ok_or_else(|| Error::Config(format!("no endpoint is named '{endpoint}'")))?;
    let config = row.to_endpoint().map_err(|refusal| {
        Error::Config(format!(
            "endpoint '{endpoint}' is not served, so its list is not read: {refusal}"
        ))
    })?;
    let (auth, platform) = (config.auth, config.endpoint.platform());
    let Some(base_url) = config.base_url.filter(|_| platform == Platform::Plain) else {
        return Err(Error::Config(format!(
            "endpoint '{endpoint}' is on the {} platform, whose model list this release \
             does not read",
            platform.as_str()
        )));
    };
    let spec = EndpointSpec::new(config.endpoint, base_url, auth, config.extra_headers)
        .map_err(Error::Config)?;
    let (account, proxy, material) = match repo::endpoint_account(db, endpoint, account).await? {
        Some(credential) => {
            let material = kek
                .open_json::<SecretMaterial>(&credential.sealed())?
                .trimmed();
            (Some(credential.name), credential.proxy_url, Some(material))
        }
        None => match account {
            Some(name) => {
                return Err(Error::Config(format!(
                    "endpoint '{endpoint}' has no credential named '{name}'"
                )));
            }
            // A host that trusts its network takes no key, and needs none.
            None if auth == AuthStyle::None => (None, None, None),
            None => {
                return Err(Error::Config(format!(
                    "endpoint '{endpoint}' has no schedulable credential to read its model \
                     list with; add one with `oag admin account add --provider {endpoint}`, \
                     or name one with --account"
                )));
            }
        },
    };
    Ok(Reader {
        label: row.display_name.unwrap_or_else(|| row.name.clone()),
        name: row.name,
        discover: row.discover_models,
        spec,
        account,
        proxy,
        material,
    })
}

/// What an `oag admin endpoint sync` asks for.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// The credential to read the list with, by name. Defaults to the
    /// endpoint's first schedulable one.
    pub account: Option<String>,
    /// Where the priced list is, when it is not where
    /// [`listing::priced`] looks. Must be on the endpoint's own origin.
    pub listing_url: Option<String>,
    /// Globs (`*`, `?`) over a model's upstream id or its catalog id. With
    /// any `include`, only the models one matches are managed, and never one
    /// an `exclude` matches: the rest are neither written nor removed.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub price: PriceChoice,
    /// Report what would change, and write nothing.
    pub dry_run: bool,
}

/// What a sync found, and did or, on a dry run, would do. Every list is of
/// catalog ids but `skipped` and `filtered`, which are of the list's own names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub endpoint: String,
    /// Where the priced list was read: its first page.
    pub url: String,
    pub pages: usize,
    /// The credential it was read with.
    pub account: Option<String>,
    pub price: PriceChoice,
    pub dry_run: bool,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    /// No longer offered, and removed.
    pub removed: Vec<String>,
    /// No longer offered, and kept because a route's ladder names them.
    pub kept_on_ladder: Vec<String>,
    /// Ids a row of another provider already holds, left as they were.
    pub held: Vec<String>,
    /// Listed and not offered for chat at a price, each with why.
    pub skipped: Vec<(String, Skip)>,
    /// Left out by `--include` or `--exclude`: neither written nor removed.
    pub filtered: Vec<String>,
}

/// Write an endpoint's priced model list into the catalog.
///
/// Each model the list offers for chat at a price becomes, or rewrites, the
/// override row `<endpoint>/<upstream id>`, with the chosen vendor's prices,
/// window and capabilities and, where the row has none, a label naming the
/// model and the endpoint. A row of the endpoint's the list no longer offers is
/// removed, unless a route's ladder names it, in which case it is kept and
/// reported: a sync never takes a model out from under a ladder. Rows a filter
/// leaves out are left alone either way.
///
/// A list that offers nothing writes nothing, and removes nothing: that is a
/// list this sync cannot read (a vocabulary it does not know, a filter that
/// matched nothing), not an endpoint that stopped serving everything.
///
/// Everything is written in one transaction. The gateway serves the rows from
/// its next catalog refresh; nothing has to restart.
pub async fn sync(db: &Db, kek: &Kek, endpoint: &str, options: &SyncOptions) -> Result<SyncReport> {
    let reader = reader(db, kek, endpoint, options.account.as_deref()).await?;
    let listed = listing::priced(
        &reader.spec.model_source(),
        options.listing_url.as_deref(),
        reader.key(),
        reader.proxy.as_deref(),
    )
    .await?;
    let existing = repo::provider_models(db, &reader.name).await?;
    let laddered = repo::laddered_models(db).await?;
    let Plan {
        mut report,
        writes,
        remove,
    } = plan(
        &reader.name,
        &reader.label,
        &listed.models,
        &existing,
        &laddered,
        options,
    )
    .map_err(|nothing| {
        Error::Config(format!(
            "the model list at {} offers no chat model this sync can price ({nothing}), so \
             nothing was written or removed",
            listed.url
        ))
    })?;
    report.url = listed.url;
    report.pages = listed.pages;
    report.account = reader.account;
    if options.dry_run {
        return Ok(report);
    }

    let done = repo::sync_endpoint_models(db, &reader.name, &writes, &remove).await?;
    // Held between the plan's look and the write: not written, so not added
    // or updated.
    for id in done.held {
        report.added.retain(|a| *a != id);
        report.updated.retain(|u| *u != id);
        report.held.push(id);
    }
    // A ladder that came to name a stale row after the plan looked keeps it.
    let removed: HashSet<&String> = done.removed.iter().collect();
    let (gone, kept): (Vec<String>, Vec<String>) =
        remove.into_iter().partition(|id| removed.contains(id));
    report.removed = gone;
    report.kept_on_ladder.extend(kept);
    Ok(report)
}

/// What a sync would write and remove, before it does.
#[derive(Debug)]
struct Plan {
    report: SyncReport,
    writes: Vec<ModelRow>,
    /// Stale rows no ladder names.
    remove: Vec<String>,
}

/// Decide a sync against the endpoint's rows as they stand, or, when the list
/// offers nothing to write, say how many entries it skipped and filtered out.
///
/// `existing` holds every row whose provider is the endpoint and every row
/// whose id carries its prefix; one of another provider holds its id, and is
/// neither written nor removed.
fn plan(
    endpoint: &str,
    label: &str,
    models: &[ListedModel],
    existing: &[StoredModelRow],
    laddered: &HashSet<String>,
    options: &SyncOptions,
) -> std::result::Result<Plan, String> {
    let filter = Filter {
        include: &options.include,
        exclude: &options.exclude,
    };
    let by_id: HashMap<&str, &StoredModelRow> =
        existing.iter().map(|r| (r.model.id.as_str(), r)).collect();
    let mut report = SyncReport {
        endpoint: endpoint.to_owned(),
        price: options.price,
        dry_run: options.dry_run,
        ..SyncReport::default()
    };
    let mut writes = Vec::new();
    let mut offered = HashSet::new();
    for model in models {
        if let Some(name) = model.upstream.as_deref()
            && !filter.admits(name, &format!("{endpoint}/{name}"))
        {
            report.filtered.push(name.to_owned());
            continue;
        }
        let offer = match listing::choose(model, options.price) {
            Ok(offer) => offer,
            Err(skip) => {
                let name = model.upstream.as_deref().unwrap_or("(unnamed)");
                report.skipped.push((name.to_owned(), skip));
                continue;
            }
        };
        let row = catalog_row(endpoint, label, &offer);
        // Listed twice: the first entry is the one written.
        if !offered.insert(row.id.clone()) {
            continue;
        }
        match by_id.get(row.id.as_str()) {
            None => {
                report.added.push(row.id.clone());
                writes.push(row);
            }
            Some(stored) if stored.model.provider != endpoint => report.held.push(row.id),
            Some(stored) if unchanged(stored, &row) => report.unchanged.push(row.id),
            Some(_) => {
                report.updated.push(row.id.clone());
                writes.push(row);
            }
        }
    }
    if offered.is_empty() {
        return Err(format!(
            "{} skipped, {} filtered out",
            report.skipped.len(),
            report.filtered.len()
        ));
    }
    let (kept, remove): (Vec<String>, Vec<String>) = existing
        .iter()
        .map(|r| &r.model)
        .filter(|m| {
            m.provider == endpoint
                && !offered.contains(&m.id)
                && filter.admits(&m.upstream_name, &m.id)
        })
        .map(|m| m.id.clone())
        .partition(|id| laddered.contains(id));
    report.kept_on_ladder = kept;
    report.removed.clone_from(&remove);
    Ok(Plan {
        report,
        writes,
        remove,
    })
}

/// The catalog row an offer becomes under `endpoint`.
fn catalog_row(endpoint: &str, label: &str, offer: &Offer) -> ModelRow {
    let shown = offer.display_name.as_deref().unwrap_or(&offer.upstream);
    ModelRow {
        id: format!("{endpoint}/{}", offer.upstream),
        provider: endpoint.to_owned(),
        upstream_name: offer.upstream.clone(),
        input_per_mtok: offer.price.input,
        output_per_mtok: offer.price.output,
        cache_read_per_mtok: offer.price.cache_read,
        cache_write_per_mtok: offer.price.cache_write,
        context_window: offer.context_window,
        max_output_tokens: offer.max_output_tokens,
        supports_vision: offer.vision,
        supports_tools: offer.tools,
        supports_reasoning: offer.reasoning,
        // A cache read is only billed where the list prices one.
        supports_prompt_cache: offer.price.cache_read.is_some(),
        display_label: Some(format!("{shown} ({label})")),
    }
}

/// Whether writing `row` over `stored` would change nothing: the same numbers
/// and flags, already an override, and a label wherever the sync has one to
/// give.
fn unchanged(stored: &StoredModelRow, row: &ModelRow) -> bool {
    let m = &stored.model;
    stored.is_override
        && m.upstream_name == row.upstream_name
        && m.input_per_mtok == row.input_per_mtok
        && m.output_per_mtok == row.output_per_mtok
        && m.cache_read_per_mtok == row.cache_read_per_mtok
        && m.cache_write_per_mtok == row.cache_write_per_mtok
        && m.context_window == row.context_window
        && m.max_output_tokens == row.max_output_tokens
        && m.supports_vision == row.supports_vision
        && m.supports_tools == row.supports_tools
        && m.supports_reasoning == row.supports_reasoning
        && m.supports_prompt_cache == row.supports_prompt_cache
        && (m.display_label.is_some() || row.display_label.is_none())
}

/// Which models a sync manages: with no `include`, all of them; with one, only
/// those an `include` matches; and never one an `exclude` matches. A pattern
/// matches a model when it matches its upstream id (`zai/*`) or its catalog id
/// (`merge/zai/*`). `*` is any run of characters, slashes included, and `?`
/// any one.
struct Filter<'a> {
    include: &'a [String],
    exclude: &'a [String],
}

impl Filter<'_> {
    fn admits(&self, upstream: &str, id: &str) -> bool {
        let hit = |pattern: &String| glob(pattern, upstream) || glob(pattern, id);
        (self.include.is_empty() || self.include.iter().any(hit)) && !self.exclude.iter().any(hit)
    }
}

/// Whether `text` matches `pattern`, where `*` is any run of characters and
/// `?` any one. A mismatch backtracks to the last `*` alone, which is enough
/// when those are the only wildcards.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    // The last `*` seen, and where in `text` it was last tried from.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// What `oag admin endpoint models` prints: the endpoint's list as discovery
/// reads it, beside the catalog's rows for the endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsReport {
    pub endpoint: String,
    /// Whether the usage poller records this list for the endpoint's keys.
    pub discover: bool,
    pub account: Option<String>,
    pub url: String,
    pub pages: usize,
    /// Each id the list names, with the catalog id it is served under, if the
    /// catalog has one.
    pub listed: Vec<(String, Option<String>)>,
    /// The endpoint's catalog rows the list does not name: the ones discovery
    /// would hide.
    pub unlisted: Vec<String>,
}

/// Read an endpoint's model list as discovery does, and write nothing.
pub async fn models(
    db: &Db,
    kek: &Kek,
    endpoint: &str,
    account: Option<&str>,
) -> Result<ModelsReport> {
    let reader = reader(db, kek, endpoint, account).await?;
    let served = listing::served(
        &reader.spec.model_source(),
        reader.key(),
        reader.proxy.as_deref(),
    )
    .await?;
    let rows: Vec<ModelRow> = repo::provider_models(db, &reader.name)
        .await?
        .into_iter()
        .map(|r| r.model)
        .filter(|m| m.provider == reader.name)
        .collect();
    let by_upstream: HashMap<&str, &str> = rows
        .iter()
        .map(|m| (m.upstream_name.as_str(), m.id.as_str()))
        .collect();
    let named: HashSet<&str> = served.models.iter().map(String::as_str).collect();
    Ok(ModelsReport {
        listed: served
            .models
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    by_upstream.get(id.as_str()).map(|c| (*c).to_owned()),
                )
            })
            .collect(),
        unlisted: rows
            .iter()
            .filter(|m| !named.contains(m.upstream_name.as_str()))
            .map(|m| m.id.clone())
            .collect(),
        endpoint: reader.name,
        discover: reader.discover,
        account: reader.account,
        url: served.url,
        pages: served.pages,
    })
}

#[cfg(test)]
mod tests;
