//! `oag admin catalog` and `providers`: the model catalog, its prices, and the provider matrix.

use super::accounts::{parse_provider, price_account, register_endpoints};
use super::{CatalogAddArgs, CatalogCommand};
use oag_core::{Kek, Provider, Result, credential::SecretMaterial};
use oag_store::{Db, ModelRow, repo};
use rust_decimal::{Decimal, RoundingStrategy};

pub(super) async fn catalog_cmd(db: &Db, kek: &Kek, cmd: CatalogCommand) -> Result<()> {
    match cmd {
        CatalogCommand::Add { args } => add_model(db, &args).await,
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
        Some(source) => {
            // A LiteLLM file keeps only the providers that parse, and an
            // endpoint's name parses once it is registered: an endpoint named
            // for a LiteLLM provider (`groq`, `openrouter`) takes its models.
            // One the gateway would not serve is not registered, so its
            // models stay out, as they would from the gateway's catalog.
            let _not_served = register_endpoints(db).await?;
            crate::catalog::from_litellm(source).await?
        }
        None => crate::catalog::builtin(),
    };
    let n = entries.len();
    for m in &entries {
        repo::upsert_model(db, m, false).await?;
    }
    println!("catalog: {n} models");
    Ok(())
}

/// A catalog id's provider and model, split at the first `/`: the model half
/// may hold more of them (`merge/zai/glm-5.3-flash` is endpoint merge's
/// `zai/glm-5.3-flash`), and a provider's name never does.
pub(super) fn split_model_id(id: &str) -> Result<(&str, &str)> {
    match id.split_once('/') {
        Some((provider, model))
            if !provider.is_empty()
                && !model.is_empty()
                && !id.chars().any(|c| c.is_whitespace() || c.is_control()) =>
        {
            Ok((provider, model))
        }
        _ => Err(oag_core::Error::Config(format!(
            "model id `{id}` is not `<provider>/<model>`"
        ))),
    }
}

/// One more than the most a price column holds: they are `numeric(12,6)`.
const PRICE_CEILING: Decimal = Decimal::from_parts(1_000_000, 0, 0, false, 0);

/// A price the catalog can hold, as it will hold it, or why not.
///
/// Rounded first, to the column's six places and half away from zero, which
/// is how Postgres rounds a value into a `numeric(12,6)`, so every rule below
/// judges the price that will be stored. Judged as typed, `0.0000004` passed as
/// a price and was stored as zero, a free model nobody said `--free` for, and
/// `999999.9999995` passed the ceiling and was refused by the database.
fn price(flag: &str, value: Decimal) -> Result<Decimal> {
    let value = value.round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero);
    // A negative that rounds to nothing is a zero, and is stored as one.
    let value = if value.is_zero() {
        Decimal::ZERO
    } else {
        value
    };
    if value < Decimal::ZERO {
        return Err(oag_core::Error::Config(format!(
            "--{flag} cannot be negative"
        )));
    }
    if value >= PRICE_CEILING {
        return Err(oag_core::Error::Config(format!(
            "--{flag} must be under {PRICE_CEILING} USD per million tokens"
        )));
    }
    Ok(value)
}

/// The row `catalog add` writes for `provider`, or the first flag that cannot
/// be stored.
///
/// A price of zero in and zero out needs `--free`. The router ranks by cost,
/// so a model that costs nothing wins every comparison on every ladder it is
/// on (`crate::catalog` skips LiteLLM's zero-priced rows for the same reason):
/// a zero typed by mistake would quietly take all the traffic it can.
pub(super) fn model_row(args: &CatalogAddArgs, provider: Provider) -> Result<ModelRow> {
    let (prefix, model) = split_model_id(&args.id)?;
    if provider.as_str() != prefix {
        return Err(oag_core::Error::Config(format!(
            "model id `{}` names its provider as `{prefix}`, which is spelt `{provider}`: \
             use --id {provider}/{model}",
            args.id
        )));
    }
    let upstream = args.upstream.trim();
    if upstream.is_empty() {
        return Err(oag_core::Error::Config(
            "--upstream is the name the provider knows the model by, and cannot be empty"
                .to_owned(),
        ));
    }
    let input = price("input-per-mtok", args.input_per_mtok.unwrap_or_default())?;
    let output = price("output-per-mtok", args.output_per_mtok.unwrap_or_default())?;
    let costs_nothing = input.is_zero() && output.is_zero();
    if costs_nothing && !args.free {
        return Err(oag_core::Error::Config(
            "a model priced at zero in and zero out wins every cost comparison, so it would \
             take every request its ladder can give it. Pass --free if it really costs nothing"
                .to_owned(),
        ));
    }
    if args.free && !costs_nothing {
        return Err(oag_core::Error::Config(
            "--free says the model costs nothing, but a price was given: drop one or the other"
                .to_owned(),
        ));
    }
    let display_label = match args.display_label.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(label) if label.chars().any(char::is_control) => {
            return Err(oag_core::Error::Config(
                "--display-label must not contain control characters".to_owned(),
            ));
        }
        Some(label) => Some(label.to_owned()),
    };
    Ok(ModelRow {
        id: args.id.clone(),
        provider: provider.as_str().to_owned(),
        upstream_name: upstream.to_owned(),
        input_per_mtok: input,
        output_per_mtok: output,
        cache_read_per_mtok: args
            .cache_read_per_mtok
            .map(|p| price("cache-read-per-mtok", p))
            .transpose()?,
        cache_write_per_mtok: args
            .cache_write_per_mtok
            .map(|p| price("cache-write-per-mtok", p))
            .transpose()?,
        context_window: args.context,
        max_output_tokens: args.max_output,
        supports_vision: args.vision,
        supports_tools: args.tools,
        supports_reasoning: args.reasoning,
        supports_prompt_cache: args.prompt_cache,
        display_label,
    })
}

/// `catalog add`: one model, as the operator states it, marked theirs.
pub(super) async fn add_model(db: &Db, args: &CatalogAddArgs) -> Result<()> {
    let (prefix, _) = split_model_id(&args.id)?;
    let provider = parse_provider(db, prefix).await?;
    let row = model_row(args, provider)?;
    repo::override_model(db, &row).await?;
    for line in added_model_lines(&row) {
        println!("{line}");
    }
    Ok(())
}

/// What `catalog add` prints.
pub(super) fn added_model_lines(row: &ModelRow) -> Vec<String> {
    let mut lines = vec![
        format!(
            "catalog: {} ({}, upstream {}) at ${} in, ${} out per Mtok; {} context, {} max output",
            row.id,
            row.provider,
            row.upstream_name,
            row.input_per_mtok,
            row.output_per_mtok,
            row.context_window,
            row.max_output_tokens
        ),
        "  an operator override: `catalog seed` and `catalog sync-prices` leave it alone"
            .to_owned(),
    ];
    if row.input_per_mtok.is_zero() && row.output_per_mtok.is_zero() {
        lines.push(
            "  free on purpose: it wins every cost comparison on a ladder that names it".to_owned(),
        );
    }
    lines.push(
        "  a running gateway routes it from its next catalog refresh; put it on a ladder \
         with `oag admin route tiers`"
            .to_owned(),
    );
    lines
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
