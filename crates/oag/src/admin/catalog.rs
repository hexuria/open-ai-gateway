//! `oag admin catalog` and `providers`: the model catalog, its prices, and the provider matrix.

use super::CatalogCommand;
use super::accounts::price_account;
use oag_core::{Kek, Result, credential::SecretMaterial};
use oag_store::{Db, repo};

pub(super) async fn catalog_cmd(db: &Db, kek: &Kek, cmd: CatalogCommand) -> Result<()> {
    match cmd {
        CatalogCommand::Seed { from } => seed_catalog(db, from.as_deref()).await,
        CatalogCommand::SyncPrices { provider, account } => {
            sync_prices(db, kek, &provider, account.as_deref()).await
        }
        CatalogCommand::List { provider, limit } => {
            list_catalog(db, provider.as_deref(), limit).await
        }
    }
}

/// What to say when a catalog listing has nothing to show.
///
/// Two different emptinesses, and they had one message between them.
/// `catalog list --provider xai` against a catalog full of Anthropic models
/// said "catalog is empty; seed it with `oag admin catalog seed`" — so the
/// operator seeded a catalog that was already seeded, got the same message, and
/// concluded the seed was broken. The filter is the answer and it was in the
/// arguments the whole time.
///
/// Its own function so the decision can be tested: capturing stdout is a
/// fixture larger than the thing it would prove.
pub(super) fn empty_catalog_lines(
    total_before_filter: usize,
    provider: Option<&str>,
) -> Vec<String> {
    match provider {
        Some(p) if total_before_filter > 0 => vec![
            format!(
                "no {p} models in the catalog, though it holds {total_before_filter} \
                 from other providers"
            ),
            "  `oag admin catalog list` shows them all".to_owned(),
        ],
        _ => vec!["catalog is empty; seed it with `oag admin catalog seed`".to_owned()],
    }
}

/// Everything `catalog list` decides, given what the catalog holds.
///
/// Separated from the printing so the filter and the two emptinesses are
/// reachable from a test. `empty_catalog_lines` below is the decision; this is
/// the code that has to feed it the count from *before* the filter, and that
/// was the half nothing exercised — pass `rows.len()` after filtering instead
/// and every assertion on the helper still passes while the command tells the
/// operator to seed a catalog that is already full.
pub(super) fn catalog_lines(
    mut rows: Vec<oag_store::ModelRow>,
    provider: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<String>> {
    let total_before_filter = rows.len();
    if let Some(p) = provider {
        let want: oag_core::Provider = p.parse()?;
        rows.retain(|m| m.provider == want.as_str());
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let total = rows.len();
    if let Some(n) = limit {
        rows.truncate(n);
    }
    if rows.is_empty() {
        // Which of the two emptinesses this is. `--provider xai` against a
        // catalog full of Anthropic models used to print "catalog is empty;
        // seed it with `oag admin catalog seed`" — so the operator seeded a
        // catalog that was already seeded, got the same message, and concluded
        // the seed was broken. The filter is the answer and it is right there
        // in the arguments.
        return Ok(empty_catalog_lines(total_before_filter, provider));
    }

    let mut lines = vec![format!(
        "{:<36} {:<12} {:>8} {:>8} {:>8}",
        "ID", "PROVIDER", "IN/MTok", "OUT/MTok", "CTX"
    )];
    for m in &rows {
        lines.push(format!(
            "{:<36} {:<12} {:>8} {:>8} {:>8}",
            m.id, m.provider, m.input_per_mtok, m.output_per_mtok, m.context_window
        ));
    }
    if rows.len() < total {
        lines.push(format!(
            "({} of {total}; pass --limit to see more or less)",
            rows.len()
        ));
    }
    Ok(lines)
}

async fn list_catalog(db: &Db, provider: Option<&str>, limit: Option<usize>) -> Result<()> {
    for line in catalog_lines(repo::catalog(db).await?, provider, limit)? {
        println!("{line}");
    }
    Ok(())
}

pub(super) async fn print_providers(db: &Db) -> Result<()> {
    let counts: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT provider, kind, COUNT(*) FROM account GROUP BY provider, kind ORDER BY provider, kind",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("counting credentials: {e}")))?;

    println!("PROVIDER     DIALECT                      ACCOUNTS         SUBSCRIPTION");
    for &p in oag_core::Provider::ALL {
        let s = p.support();
        let n: i64 = counts
            .iter()
            .filter(|c| c.0 == p.as_str())
            .map(|c| c.2)
            .sum();
        let sub = match s.subscription {
            oag_core::provider::SubscriptionSupport::Served { import }
            | oag_core::provider::SubscriptionSupport::CredentialImportOnly { import, .. } => {
                import
            }
            oag_core::provider::SubscriptionSupport::NotOffered { .. } => "no",
            _ => "unknown",
        };
        println!(
            "{:<12} {:<28} {:<16} {sub}",
            p.as_str(),
            s.dialect().as_str(),
            n,
        );
        if let Some(note) = s.note {
            println!("             {note}");
        }
    }
    Ok(())
}

pub(super) async fn seed_catalog(db: &Db, from: Option<&str>) -> Result<()> {
    let entries = match from {
        Some(source) => crate::catalog::from_litellm(source).await?,
        None => crate::catalog::builtin(),
    };
    let n = entries.len();
    for m in &entries {
        repo::upsert_model(db, m, false).await?;
    }
    println!("catalog: {n} models");
    Ok(())
}

pub(super) async fn sync_prices(
    db: &Db,
    kek: &Kek,
    provider: &str,
    account: Option<&str>,
) -> Result<()> {
    let known: oag_core::Provider = provider.parse()?;
    let row = price_account(db, known, account).await?;
    let material: SecretMaterial = kek.open_json::<SecretMaterial>(&row.sealed())?.trimmed();

    // The kind as well as the provider: a subscription seat's token is not a
    // management-API key, and presenting it to a price endpoint gets a 401 that
    // reads as an auth failure against a credential working perfectly well for
    // inference.
    let Some(kind) = oag_core::credential::CredentialKind::from_column(&row.kind) else {
        return Err(oag_core::Error::Internal(format!(
            "credential '{}' has an unknown kind '{}'",
            row.name, row.kind
        )));
    };
    let Some(prices) =
        oag_upstream::pricing::fetch(known, kind, &material, row.proxy_url.as_deref()).await?
    else {
        return Err(oag_core::Error::Config(format!(
            "{known} publishes no price API for a {kind:?} credential; seed it from \
             LiteLLM instead, or name an API-key credential with --account"
        )));
    };

    // The whole catalog, not one lookup per model: this is a handful of rows
    // against a table with a few thousand in it, and the ids are the only part
    // that matters.
    let existing: std::collections::HashSet<String> =
        repo::catalog(db).await?.into_iter().map(|m| m.id).collect();

    let (mut repriced, mut added, mut overridden) = (0u32, 0u32, 0u32);
    for change in crate::catalog::plan_price_sync(known, &prices, &existing) {
        match change {
            crate::catalog::PriceSync::Reprice {
                id,
                input_per_mtok,
                output_per_mtok,
                cache_read_per_mtok,
            } => {
                if repo::update_model_prices(
                    db,
                    &id,
                    input_per_mtok,
                    output_per_mtok,
                    cache_read_per_mtok,
                )
                .await?
                {
                    repriced += 1;
                } else {
                    // The row exists — it came out of the catalog a moment ago
                    // — so the only thing that can have skipped it is the
                    // operator override guard.
                    overridden += 1;
                }
            }
            crate::catalog::PriceSync::Insert(m) => {
                repo::upsert_model(db, &m, false).await?;
                added += 1;
            }
        }
    }

    println!(
        "{known} via {}: {repriced} repriced, {added} added, {overridden} left to the operator",
        row.name
    );
    if added > 0 {
        println!(
            "  new rows carry a conservative context window until a LiteLLM seed \
             fills in the real one"
        );
    }
    Ok(())
}
