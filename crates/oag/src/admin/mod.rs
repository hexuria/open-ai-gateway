//! `oag admin` — the operations a human runs from a shell.
//!
//! Enough to stand up a working gateway without the UI existing yet, and to
//! recover one when the UI is the thing that is broken.
//!
//! Noun-verb grouping (`account add`, `key create`, `catalog seed`) is the
//! surface `--help` shows. The old flat spellings remain as hidden clap
//! aliases so existing scripts keep working without a deprecation line on
//! every CI call.

mod doctor;

use clap::{Args, Subcommand, ValueEnum};
use oag_core::config::Config;
use oag_core::{Kek, Result, credential::SecretMaterial};
use oag_store::{Db, repo};
use rand::Rng;
use rust_decimal::Decimal;
use uuid::Uuid;

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Create the first principal, a default route, and an API key.
    ///
    /// Idempotent on the principal and route; always mints a new key.
    Init {
        #[arg(long, default_value = "admin@localhost")]
        email: String,
        #[arg(long, default_value = "default")]
        route: String,
        /// Monthly spend cap in USD. Omit for uncapped.
        #[arg(long)]
        budget_usd: Option<Decimal>,
    },
    /// Show routes, credentials, and this month's spend.
    Status,
    /// Check why a request on this route would fail.
    Doctor {
        #[arg(long, default_value = "default")]
        route: String,
    },
    /// Print the provider support matrix.
    Providers,
    /// Upstream credentials.
    #[command(subcommand)]
    Account(AccountCommand),
    /// Inbound API keys.
    Key(KeyCli),
    /// Routing policy for a named route.
    #[command(subcommand)]
    Route(RouteCommand),
    /// Model catalog: seed, overlay prices, list.
    #[command(subcommand)]
    Catalog(CatalogCommand),
    /// Shared caches.
    #[command(subcommand)]
    Cache(CacheCommand),

    // Hidden spellings of the pre-redesign flat commands. `hide = true` keeps
    // them out of `--help`; clap still parses them so existing scripts do not
    // break. No deprecation line: these run from CI.
    /// Register an upstream credential.
    #[command(hide = true)]
    AddAccount {
        #[command(flatten)]
        args: AccountAddArgs,
    },
    /// Load model pricing into the catalog.
    #[command(hide = true)]
    SeedCatalog {
        #[arg(long)]
        from: Option<String>,
    },
    /// Overlay a provider's own prices onto the catalog.
    #[command(hide = true)]
    SyncPrices {
        #[arg(long, default_value = "xai")]
        provider: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Choose whether a concrete model name is honoured or overridden.
    #[command(hide = true)]
    SetMode {
        #[arg(long, default_value = "default")]
        route: String,
        #[arg(long)]
        mode: String,
    },
    /// Set a route's tier ladder from JSON.
    #[command(hide = true)]
    SetTiers {
        #[arg(long, default_value = "default")]
        route: String,
        /// `[{"name":"cheap","models":["kimi/k2"]}, ...]`, cheapest first.
        #[arg(long)]
        tiers: String,
    },
    /// Revoke an inbound key by its displayed prefix.
    #[command(hide = true)]
    RevokeKey {
        #[arg(long)]
        prefix: String,
    },
    /// Drop the shared auth cache.
    #[command(hide = true)]
    FlushCache,
}

/// A CLI whose session we can import as an OAuth seat.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum AccountSource {
    /// Grok CLI (`~/.grok/auth.json`).
    Grok,
    /// Codex CLI (`~/.codex/auth.json`).
    Codex,
}

#[derive(Subcommand, Debug)]
pub enum AccountCommand {
    /// Register an upstream credential.
    Add {
        #[command(flatten)]
        args: AccountAddArgs,
    },
    /// List upstream credentials.
    List,
    /// Take a credential out of rotation.
    Disable {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Put a credential back into rotation.
    Enable {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Correct a seat's flat monthly price.
    ///
    /// The one figure nothing can infer — a provider's API reports how much of
    /// a plan is left, never what the plan costs you — so it is typed in by
    /// hand at import, and a hand-typed number is eventually a wrong one. It
    /// feeds the savings column, so until this existed the only way to fix a
    /// mistyped price was an UPDATE against the database.
    SetCost {
        #[arg(value_name = "NAME")]
        name: String,
        /// The seat's price per month in USD. Omit to clear it, which makes the
        /// saving read as unknown rather than as a saving of the whole fee.
        #[arg(long)]
        monthly_cost: Option<Decimal>,
    },
}

#[derive(Args, Debug)]
pub struct AccountAddArgs {
    #[arg(long)]
    name: String,
    /// Not needed with `--from`, which knows the provider.
    #[arg(
        long,
        required_unless_present_any = ["from", "from_grok", "from_codex"],
        conflicts_with_all = ["from", "from_grok", "from_codex"]
    )]
    provider: Option<String>,
    /// The provider API key. Read from `OAG_ACCOUNT_SECRET` if omitted, so it
    /// need not appear in shell history or the process table.
    #[arg(
        long,
        env = "OAG_ACCOUNT_SECRET",
        hide_env_values = true,
        required_unless_present_any = ["from", "from_grok", "from_codex"],
        conflicts_with_all = ["from", "from_grok", "from_codex"]
    )]
    secret: Option<String>,
    /// Import a signed-in CLI session as an OAuth credential.
    ///
    /// `grok` reads `~/.grok/auth.json`, `codex` reads `~/.codex/auth.json`.
    /// Override with `--auth-file`. Never writes the source file.
    #[arg(long, value_enum)]
    from: Option<AccountSource>,
    /// Hidden spelling of `--from grok`.
    #[arg(long, hide = true, conflicts_with_all = ["from", "from_codex"])]
    from_grok: bool,
    /// Hidden spelling of `--from codex`.
    #[arg(long, hide = true, conflicts_with_all = ["from", "from_grok"])]
    from_codex: bool,
    /// Where to read CLI sessions from. Repeatable; the first file a token
    /// appears in wins.
    #[arg(long)]
    auth_file: Vec<String>,
    #[arg(long, default_value = "default")]
    route: String,
    #[arg(long, default_value_t = 8)]
    max_concurrency: i32,
    #[arg(long, default_value_t = 0)]
    priority: i16,
    /// Bind to one principal instead of the shared pool. See
    /// docs/compliance.md.
    #[arg(long)]
    owner_email: Option<String>,
    /// Put an OAuth seat in the shared pool anyway. Deliberate opt-in:
    /// subscription seats are sanctioned for the holder's own use, so the
    /// default for an imported seat is per-principal binding.
    #[arg(long, conflicts_with = "owner_email")]
    shared: bool,
    /// The seat's flat monthly price in USD. Lets the dashboard net a
    /// subscription's saved API spend against what it costs. Applies per
    /// imported seat.
    #[arg(long)]
    monthly_cost: Option<Decimal>,
}

/// `oag admin key` is a group (`create`/`list`/`revoke`) and, with no
/// subcommand, the old `key --email` form. Flattened flags are hidden so
/// `--help` only shows the group. Defaults live in the handler rather than
/// clap: `default_value` on a parent arg makes clap treat it as present, which
/// then fights the subcommand.
#[derive(Args, Debug)]
pub struct KeyCli {
    #[command(subcommand)]
    action: Option<KeyAction>,
    #[arg(long, hide = true)]
    email: Option<String>,
    #[arg(long, hide = true)]
    route: Option<String>,
    #[arg(long, hide = true)]
    name: Option<String>,
    #[arg(long, hide = true)]
    floor_tier: Option<String>,
    #[arg(long, hide = true)]
    admin: bool,
}

#[derive(Subcommand, Debug)]
pub enum KeyAction {
    /// Mint an API key for an existing principal and route.
    Create {
        #[arg(long)]
        email: String,
        #[arg(long, default_value = "default")]
        route: String,
        #[arg(long, default_value = "cli")]
        name: String,
        /// Never route below this tier, whatever the classifier says.
        #[arg(long)]
        floor_tier: Option<String>,
        /// Mint an admin key: one that can reach the admin API and perform
        /// writes. Deliberately opt-in — an inference key must not be able to
        /// disable credentials just because its owner happens to be an admin.
        #[arg(long)]
        admin: bool,
    },
    /// List inbound keys (prefix, never the secret).
    List,
    /// Revoke an inbound key by its displayed prefix.
    ///
    /// The one write that genuinely needs a CLI: during an incident the prefix
    /// is what an operator can actually see (in a log, in the dashboard), and
    /// psql alone cannot evict the shared auth cache, so a row update there
    /// leaves the key working on every replica for up to its cache TTL.
    Revoke {
        #[arg(value_name = "PREFIX")]
        prefix: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum RouteCommand {
    /// Choose whether a concrete model name is honoured or overridden.
    Mode {
        /// `passthrough` honours a named model; `managed` applies policy to
        /// every request. Virtual `oag/*` names are always managed.
        mode: RouteMode,
        #[arg(long, default_value = "default")]
        route: String,
    },
    /// Set a route's tier ladder.
    ///
    /// Positional `cheap=m1,m2 balanced=m3`, cheapest first. The JSON form
    /// lives on the hidden `set-tiers` spelling.
    Tiers {
        #[arg(long, default_value = "default")]
        route: String,
        /// `cheap=xai/grok-4.3 balanced=xai/grok-4.5`, cheapest first.
        #[arg(value_name = "RUNG", required = true, num_args = 1..)]
        rungs: Vec<String>,
    },
    /// Show a route's mode and ladder.
    Show {
        #[arg(long, default_value = "default")]
        route: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RouteMode {
    Passthrough,
    Managed,
}

impl RouteMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::Managed => "managed",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum CatalogCommand {
    /// Load model pricing into the catalog.
    Seed {
        /// A LiteLLM-format `model_prices_and_context_window.json`: a local
        /// path or an http(s) URL. Omit to use the small built-in set.
        #[arg(long)]
        from: Option<String>,
    },
    /// Overlay a provider's own prices onto the catalog.
    ///
    /// A separate command rather than another `catalog seed --from`: that loads
    /// a whole catalog — prices, context windows, capabilities — from a table
    /// anyone can fetch, while this needs a stored credential, and its source
    /// is authoritative about money and silent about everything else. So it
    /// writes prices and refuses to touch a context window; folding the two
    /// together would put that refusal one forgotten flag away from a catalog
    /// full of guessed windows.
    SyncPrices {
        #[arg(long, default_value = "xai")]
        provider: String,
        /// Which credential to authenticate with. Defaults to the first
        /// schedulable one for the provider — the price list is the same for
        /// every seat, so the choice only matters when one seat's token is
        /// stale.
        #[arg(long)]
        account: Option<String>,
    },
    /// List catalog entries.
    List {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand, Debug)]
pub enum CacheCommand {
    /// Drop the shared auth cache.
    ///
    /// Budget, quota, and floor-tier changes are read through a cache, so they
    /// take up to five minutes to reach every replica. This clears the shared
    /// tier immediately; each replica's own short-lived cache expires within
    /// fifteen seconds, which bounds the rest.
    Flush,
}

pub async fn run(
    cmd: AdminCommand,
    db: &Db,
    kek: &Kek,
    redis_url: &str,
    config: &Config,
) -> Result<()> {
    match cmd {
        AdminCommand::Init {
            email,
            route,
            budget_usd,
        } => init(db, &email, &route, budget_usd).await,
        AdminCommand::Status => status(db).await,
        AdminCommand::Doctor { route } => doctor::run(db, config, &route).await,
        AdminCommand::Providers => print_providers(db).await,
        AdminCommand::Account(cmd) => account_cmd(db, kek, cmd).await,
        AdminCommand::Key(cli) => key_cmd(db, redis_url, cli).await,
        AdminCommand::Route(cmd) => route_cmd(db, cmd).await,
        AdminCommand::Catalog(cmd) => catalog_cmd(db, kek, cmd).await,
        AdminCommand::Cache(CacheCommand::Flush) | AdminCommand::FlushCache => {
            flush_cache(redis_url).await
        }
        AdminCommand::AddAccount { args } => add_account_from_args(db, kek, args).await,
        AdminCommand::SeedCatalog { from } => seed_catalog(db, from.as_deref()).await,
        AdminCommand::SyncPrices { provider, account } => {
            sync_prices(db, kek, &provider, account.as_deref()).await
        }
        AdminCommand::SetMode { route, mode } => set_mode(db, &route, &mode).await,
        AdminCommand::SetTiers { route, tiers } => set_tiers_json(db, &route, &tiers).await,
        AdminCommand::RevokeKey { prefix } => revoke_key(db, redis_url, &prefix).await,
    }
}

async fn account_cmd(db: &Db, kek: &Kek, cmd: AccountCommand) -> Result<()> {
    match cmd {
        AccountCommand::Add { args } => add_account_from_args(db, kek, args).await,
        AccountCommand::List => list_accounts(db).await,
        AccountCommand::Disable { name } => set_account_schedulable(db, &name, false).await,
        AccountCommand::Enable { name } => set_account_schedulable(db, &name, true).await,
        AccountCommand::SetCost { name, monthly_cost } => {
            set_account_cost(db, &name, monthly_cost).await
        }
    }
}

/// Set or clear a seat's monthly price.
///
/// Accepts any account rather than only a flat-rate one: a price on a metered
/// key is meaningless but harmless, and refusing it would mean explaining the
/// kinds taxonomy at the moment someone is trying to correct a typo.
async fn set_account_cost(db: &Db, name: &str, monthly_cost: Option<Decimal>) -> Result<()> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "UPDATE account SET monthly_cost_usd = $2, updated_at = now() \
         WHERE name = $1 RETURNING name, kind",
    )
    .bind(name)
    .bind(monthly_cost)
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("updating account: {e}")))?;

    let Some((_, kind)) = rows.first() else {
        return Err(oag_core::Error::Config(format!(
            "no credential named {name}; see `oag admin account list`"
        )));
    };

    match monthly_cost {
        Some(cost) => println!("{name} costs ${cost}/month"),
        None => println!("{name} has no monthly price; its saving will read as unknown"),
    }
    // Worth saying once: the figure only ever surfaces on a flat-rate line, so
    // setting it on an API key looks like it did nothing.
    if kind != "oauth" {
        println!("  note: {name} is a {kind} credential, and only subscription");
        println!("  seats are metered against a monthly price");
    }
    Ok(())
}

async fn add_account_from_args(db: &Db, kek: &Kek, args: AccountAddArgs) -> Result<()> {
    let AccountAddArgs {
        name,
        provider,
        secret,
        from,
        from_grok,
        from_codex,
        auth_file,
        route,
        max_concurrency,
        priority,
        owner_email,
        shared,
        monthly_cost,
    } = args;
    let source = if from_grok {
        Some(AccountSource::Grok)
    } else if from_codex {
        Some(AccountSource::Codex)
    } else {
        from
    };
    match source {
        Some(AccountSource::Grok) => {
            import_grok(
                db,
                kek,
                &name,
                &auth_file,
                &route,
                max_concurrency,
                priority,
                owner_email.as_deref(),
                shared,
                monthly_cost,
            )
            .await
        }
        Some(AccountSource::Codex) => {
            import_codex(
                db,
                kek,
                &name,
                &auth_file,
                &route,
                max_concurrency,
                priority,
                owner_email.as_deref(),
                shared,
                monthly_cost,
            )
            .await
        }
        None => {
            let (Some(provider), Some(secret)) = (provider, secret) else {
                return Err(oag_core::Error::Config(
                    "--provider and --secret are required without --from".to_owned(),
                ));
            };
            add_account(
                db,
                kek,
                &name,
                &provider,
                &secret,
                &route,
                max_concurrency,
                priority,
                owner_email.as_deref(),
                monthly_cost,
            )
            .await
        }
    }
}

async fn key_cmd(db: &Db, redis_url: &str, cli: KeyCli) -> Result<()> {
    match cli.action {
        Some(KeyAction::Create {
            email,
            route,
            name,
            floor_tier,
            admin,
        }) => {
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

async fn route_cmd(db: &Db, cmd: RouteCommand) -> Result<()> {
    match cmd {
        RouteCommand::Mode { mode, route } => set_mode(db, &route, mode.as_str()).await,
        RouteCommand::Tiers { route, rungs } => {
            let parsed = parse_ladder_rungs(&rungs)?;
            set_rungs(db, &route, parsed).await
        }
        RouteCommand::Show { route } => show_route(db, &route).await,
    }
}

async fn catalog_cmd(db: &Db, kek: &Kek, cmd: CatalogCommand) -> Result<()> {
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

fn parse_ladder_rungs(specs: &[String]) -> Result<Vec<oag_router::ladder::Rung>> {
    if specs.is_empty() {
        return Err(oag_core::Error::Config(
            "pass rungs as name=model[,model] cheapest first, e.g. cheap=xai/grok-4.3".to_owned(),
        ));
    }
    let mut rungs = Vec::with_capacity(specs.len());
    for spec in specs {
        let Some((name, models)) = spec.split_once('=') else {
            return Err(oag_core::Error::Config(format!(
                "expected name=model[,model], got '{spec}'"
            )));
        };
        let models: Vec<oag_router::ModelId> = models
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(oag_router::ModelId::new)
            .collect();
        if models.is_empty() {
            return Err(oag_core::Error::Config(format!(
                "rung '{name}' has no models"
            )));
        }
        rungs.push(oag_router::ladder::Rung {
            name: oag_core::TierName::from(name),
            models,
        });
    }
    Ok(rungs)
}

async fn set_rungs(db: &Db, route: &str, rungs: Vec<oag_router::ladder::Rung>) -> Result<()> {
    if oag_router::TierLadder::new(rungs.clone()).is_none() {
        return Err(oag_core::Error::Config(
            "a ladder needs at least one rung".to_owned(),
        ));
    }
    let value = serde_json::to_value(&rungs).map_err(oag_core::Error::Serde)?;
    let n = sqlx::query("UPDATE route SET tiers = $2, updated_at = now() WHERE name = $1")
        .bind(route)
        .bind(&value)
        .execute(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("setting tiers: {e}")))?;
    if n.rows_affected() == 0 {
        return Err(oag_core::Error::Config(format!("no route named {route}")));
    }
    println!("route '{route}' ladder set: {} rungs", rungs.len());
    for (i, r) in rungs.iter().enumerate() {
        println!("  {i}. {} -> {}", r.name, r.models.len());
    }
    Ok(())
}

type AccountListRow = (
    String,
    String,
    String,
    bool,
    Option<time::OffsetDateTime>,
    Option<time::OffsetDateTime>,
    i16,
);

async fn list_accounts(db: &Db) -> Result<()> {
    let rows: Vec<AccountListRow> = sqlx::query_as(
        r"
        SELECT name, provider, kind, schedulable, cooldown_until, rate_limited_until, priority
        FROM account ORDER BY provider, name
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing accounts: {e}")))?;

    if rows.is_empty() {
        println!("no credentials; add one with `oag admin account add`");
        return Ok(());
    }
    println!("NAME                 PROVIDER     KIND       STATE          PRIORITY");
    let now = time::OffsetDateTime::now_utc();
    for (name, provider, kind, schedulable, cooldown, rate_limited, priority) in rows {
        let state = if !schedulable {
            "disabled"
        } else if cooldown.is_some_and(|t| t > now) {
            "cooling down"
        } else if rate_limited.is_some_and(|t| t > now) {
            "rate limited"
        } else {
            "ready"
        };
        println!("{name:<20} {provider:<12} {kind:<10} {state:<14} {priority}");
    }
    Ok(())
}

async fn set_account_schedulable(db: &Db, name: &str, value: bool) -> Result<()> {
    let names: Vec<String> = sqlx::query_scalar(
        "UPDATE account SET schedulable = $2, updated_at = now() WHERE name = $1 RETURNING name",
    )
    .bind(name)
    .bind(value)
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("updating account: {e}")))?;
    if names.is_empty() {
        return Err(oag_core::Error::Config(format!(
            "no credential named {name}; see `oag admin account list`"
        )));
    }
    let verb = if value { "enabled" } else { "disabled" };
    println!("{verb} {name}");
    Ok(())
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

async fn show_route(db: &Db, route: &str) -> Result<()> {
    let row: Option<(String, serde_json::Value, Option<String>, Option<Decimal>)> = sqlx::query_as(
        "SELECT default_mode, tiers, floor_tier, monthly_budget_usd FROM route WHERE name = $1",
    )
    .bind(route)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("loading route: {e}")))?;
    let Some((mode, tiers, floor, budget)) = row else {
        return Err(oag_core::Error::Config(format!(
            "no route named {route}; `oag admin init` creates 'default'"
        )));
    };
    let rungs: Vec<oag_router::ladder::Rung> =
        serde_json::from_value(tiers).map_err(oag_core::Error::Serde)?;
    println!("route {route}");
    println!("  mode    {mode}");
    println!("  floor   {}", floor.as_deref().unwrap_or("(none)"));
    println!(
        "  budget  {}",
        budget.map_or_else(|| "uncapped".to_owned(), |b| format!("${b}/mo"))
    );
    println!("  ladder");
    for (i, r) in rungs.iter().enumerate() {
        let models: Vec<&str> = r.models.iter().map(oag_router::ModelId::as_str).collect();
        println!("    {i}. {} = {}", r.name, models.join(","));
    }
    Ok(())
}

async fn list_catalog(db: &Db, provider: Option<&str>, limit: Option<usize>) -> Result<()> {
    let mut rows = repo::catalog(db).await?;
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
        println!("catalog is empty; seed it with `oag admin catalog seed`");
        return Ok(());
    }
    println!(
        "{:<36} {:<12} {:>8} {:>8} {:>8}",
        "ID", "PROVIDER", "IN/MTok", "OUT/MTok", "CTX"
    );
    for m in &rows {
        println!(
            "{:<36} {:<12} {:>8} {:>8} {:>8}",
            m.id, m.provider, m.input_per_mtok, m.output_per_mtok, m.context_window
        );
    }
    if rows.len() < total {
        println!(
            "({} of {total}; pass --limit to see more or less)",
            rows.len()
        );
    }
    Ok(())
}

async fn print_providers(db: &Db) -> Result<()> {
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

async fn init(db: &Db, email: &str, route: &str, budget: Option<Decimal>) -> Result<()> {
    let principal_id = upsert_principal(db, email, "admin", budget).await?;
    let route_id = upsert_route(db, route).await?;
    println!("principal {email} -> {principal_id}");
    println!("route     {route} -> {route_id}");
    let key = mint_key(db, email, route, "initial", None, true).await?;
    print_key(&key);
    println!("\nNext:");
    println!("  This is an ADMIN key: it can disable credentials and revoke keys.");
    println!("  Do not paste it into a client. Mint a separate one for SDKs:");
    println!("      oag admin key create --email {email} --route {route} --name codex");
    println!();
    println!("  oag admin catalog seed");
    println!("  oag admin account add --name <n> --provider anthropic --secret <key>");
    println!();
    println!("  This route is in passthrough mode: a client that names a concrete");
    println!("  model gets that model. Clients asking for oag/auto are routed by");
    println!("  policy. To apply policy to every request, including ones that name");
    println!("  a model:");
    println!("      oag admin route mode managed --route {route}");
    Ok(())
}

async fn upsert_principal(
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
            role = EXCLUDED.role,
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

/// A starter ladder. Deliberately three rungs with one model each: it is the
/// smallest thing that demonstrates classification, escalation, and budget
/// downgrade all doing something.
const DEFAULT_TIERS: &str = r#"[
  {"name": "cheap",    "models": ["anthropic/claude-haiku-4.5"]},
  {"name": "balanced", "models": ["anthropic/claude-sonnet-4.5"]},
  {"name": "frontier", "models": ["anthropic/claude-opus-5"]}
]"#;

async fn upsert_route(db: &Db, name: &str) -> Result<Uuid> {
    let tiers: serde_json::Value =
        serde_json::from_str(DEFAULT_TIERS).map_err(oag_core::Error::Serde)?;
    let id: (Uuid,) = sqlx::query_as(
        r"
        INSERT INTO route (id, name, tiers, default_mode)
        VALUES ($1, $2, $3, 'passthrough')
        ON CONFLICT (name) DO UPDATE SET updated_at = now()
        RETURNING id
        ",
    )
    .bind(Uuid::now_v7())
    .bind(name)
    .bind(tiers)
    .fetch_one(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("creating route: {e}")))?;
    Ok(id.0)
}

/// Mint a key. The plaintext is returned once and never stored.
async fn mint_key(
    db: &Db,
    email: &str,
    route: &str,
    name: &str,
    floor_tier: Option<&str>,
    admin: bool,
) -> Result<String> {
    use std::fmt::Write as _;

    // 32 bytes of entropy. The prefix is there so a leaked key is recognisable
    // in a log and can be grepped for during an incident.
    let mut raw = [0u8; 32];
    rand::thread_rng().fill(&mut raw);
    let key = format!(
        "oag_live_{}",
        raw.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    );

    let hash = repo::hash_key(&key);
    let prefix: String = key.chars().take(16).collect();

    sqlx::query(
        r"
        INSERT INTO api_key
            (id, key_hash, key_prefix, name, principal_id, route_id, floor_tier, admin)
        SELECT $1, $2, $3, $4, p.id, r.id, $7, $8
        FROM principal p, route r
        WHERE p.email = $5 AND r.name = $6
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
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("minting key: {e}")))?;

    Ok(key)
}

fn print_key(key: &str) {
    println!("\n  {key}\n");
    println!("  This is shown once. Only its SHA-256 is stored, so it cannot be recovered.");
}

#[allow(clippy::too_many_arguments)]
async fn add_account(
    db: &Db,
    kek: &Kek,
    name: &str,
    provider: &str,
    secret: &str,
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_email: Option<&str>,
    monthly_cost: Option<Decimal>,
) -> Result<()> {
    // Validate before storing, so a typo fails here rather than on the first
    // request with an opaque upstream 404.
    let provider: oag_core::Provider = provider.parse()?;

    let material = SecretMaterial {
        access_token: secret.to_owned(),
        refresh_token: None,
        expires_at: None,
        version: 0,
        client_id: None,
        account_id: None,
    };

    let owner_id = find_owner(db, owner_email).await?;
    let id = insert_account(
        db,
        kek,
        name,
        provider,
        "api_key",
        &material,
        route,
        max_concurrency,
        priority,
        owner_id,
        monthly_cost,
    )
    .await?;

    println!("account {name} ({provider}) -> {id}");
    println!(
        "  sealed at rest, attached to route '{route}', {}",
        scope_of(owner_id)
    );
    Ok(())
}

/// Import every signed-in Grok CLI session as an xAI OAuth credential.
///
/// Reads the CLI's `auth.json` and never writes it: the CLI owns that file,
/// and rotated tokens land in the `account` row instead, where `ensure_fresh`
/// persists them version-guarded.
#[allow(clippy::too_many_arguments)]
async fn import_grok(
    db: &Db,
    kek: &Kek,
    name: &str,
    auth_files: &[String],
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_email: Option<&str>,
    shared: bool,
    monthly_cost: Option<Decimal>,
) -> Result<()> {
    // A subscription seat is sanctioned for its holder's own use, so binding
    // to a principal is the default and pooling is the explicit choice —
    // docs/compliance.md has the distinction this encodes.
    if owner_email.is_none() && !shared {
        return Err(oag_core::Error::Config(
            "a subscription seat binds to one principal by default: pass \
             --owner-email <email>, or --shared to pool it deliberately"
                .to_owned(),
        ));
    }

    let paths: Vec<String> = if auth_files.is_empty() {
        let home = std::env::var("HOME")
            .map_err(|_| oag_core::Error::Config("HOME is not set; pass --auth-file".to_owned()))?;
        vec![format!("{home}/.grok/auth.json")]
    } else {
        auth_files.to_vec()
    };

    let mut batches = Vec::new();
    for path in &paths {
        let json = std::fs::read_to_string(path)
            .map_err(|e| oag_core::Error::Config(format!("reading {path}: {e}")))?;
        batches.push(
            oag_upstream::xai_oauth::sessions_from_json(&json)
                .map_err(|e| oag_core::Error::Config(format!("{path}: {e}")))?,
        );
    }
    let sessions = oag_upstream::xai_oauth::union_sessions(batches);
    if sessions.is_empty() {
        return Err(oag_core::Error::Config(format!(
            "no signed-in xAI session in {}; run `grok` and log in first",
            paths.join(", ")
        )));
    }

    let owner_id = find_owner(db, owner_email).await?;
    let many = sessions.len() > 1;
    for (i, session) in sessions.into_iter().enumerate() {
        let row_name = if many {
            format!("{name}-{}", i + 1)
        } else {
            name.to_owned()
        };
        let refreshable = session.refresh_token.is_some();
        let id = insert_account(
            db,
            kek,
            &row_name,
            oag_core::Provider::XAI,
            "oauth",
            &session.into_material(),
            route,
            max_concurrency,
            priority,
            owner_id,
            monthly_cost,
        )
        .await?;
        println!("account {row_name} (xai, oauth) -> {id}");
        if !refreshable {
            println!("  no refresh token in this session: it will die at expiry");
        }
    }
    println!(
        "  sealed at rest, attached to route '{route}', {}",
        scope_of(owner_id)
    );
    println!("  auth.json was read, not written; the Grok CLI stays signed in");
    warn_if_unpriced(name, monthly_cost);
    Ok(())
}

/// Say so when a seat has no price.
///
/// The saving column nets a seat's fee against what its traffic would have cost,
/// and with no fee it can only show a dash. Import is the moment the operator
/// knows the number, so it is the moment to ask — a dash discovered weeks later
/// looks like a broken report rather than an unanswered question.
fn warn_if_unpriced(name: &str, monthly_cost: Option<Decimal>) {
    if monthly_cost.is_none() {
        println!("  no monthly price set, so this seat's saving will read as unknown");
        println!("    set it with: oag admin account set-cost {name} --monthly-cost <price>");
    }
}

/// Import the signed-in Codex CLI session as an OpenAI OAuth credential.
///
/// The mirror of `import_grok` for a ChatGPT/Codex subscription: reads the
/// CLI's `auth.json` and never writes it, storing the OAuth pair (and the
/// account id Codex sends as a header) sealed in the `account` row, where
/// `ensure_fresh` rotates it.
#[allow(clippy::too_many_arguments)]
async fn import_codex(
    db: &Db,
    kek: &Kek,
    name: &str,
    auth_files: &[String],
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_email: Option<&str>,
    shared: bool,
    monthly_cost: Option<Decimal>,
) -> Result<()> {
    // Same stance as a Grok seat: sanctioned for the holder's own use, so it
    // binds to a principal by default and pooling is the explicit choice.
    if owner_email.is_none() && !shared {
        return Err(oag_core::Error::Config(
            "a subscription seat binds to one principal by default: pass \
             --owner-email <email>, or --shared to pool it deliberately"
                .to_owned(),
        ));
    }

    let paths: Vec<String> = if auth_files.is_empty() {
        codex_auth_paths()?
    } else {
        auth_files.to_vec()
    };

    // The first path that carries a usable OAuth session wins; an API-key-only
    // auth.json parses to None and is skipped.
    let mut session = None;
    let mut tried = Vec::new();
    for path in &paths {
        let Ok(json) = std::fs::read_to_string(path) else {
            continue;
        };
        tried.push(path.clone());
        if let Some(s) = oag_upstream::openai_oauth::session_from_json(&json)
            .map_err(|e| oag_core::Error::Config(format!("{path}: {e}")))?
        {
            session = Some(s);
            break;
        }
    }
    let Some(session) = session else {
        return Err(oag_core::Error::Config(format!(
            "no signed-in Codex OAuth session in {}; run `codex` and log in with ChatGPT first",
            if tried.is_empty() {
                paths.join(", ")
            } else {
                tried.join(", ")
            }
        )));
    };

    let owner_id = find_owner(db, owner_email).await?;
    let id = insert_account(
        db,
        kek,
        name,
        oag_core::Provider::OpenAI,
        "oauth",
        &session.into_material(),
        route,
        max_concurrency,
        priority,
        owner_id,
        monthly_cost,
    )
    .await?;

    println!("account {name} (openai, oauth) -> {id}");
    println!(
        "  sealed at rest, attached to route '{route}', {}",
        scope_of(owner_id)
    );
    println!("  auth.json was read, not written; the Codex CLI stays signed in");
    warn_if_unpriced(name, monthly_cost);
    Ok(())
}

/// The default places a Codex CLI session lives, in the order the CLI itself
/// resolves them: `$CODEX_HOME`, then `~/.codex`, then `~/.config/codex`.
fn codex_auth_paths() -> Result<Vec<String>> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return Ok(vec![format!("{home}/auth.json")]);
    }
    let home = std::env::var("HOME")
        .map_err(|_| oag_core::Error::Config("HOME is not set; pass --auth-file".to_owned()))?;
    Ok(vec![
        format!("{home}/.codex/auth.json"),
        format!("{home}/.config/codex/auth.json"),
    ])
}

async fn find_owner(db: &Db, owner_email: Option<&str>) -> Result<Option<Uuid>> {
    match owner_email {
        Some(email) => {
            let row: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM principal WHERE email = $1")
                .bind(email)
                .fetch_optional(db.pool())
                .await
                .map_err(|e| oag_core::Error::Internal(format!("finding owner: {e}")))?;
            let id = row
                .ok_or_else(|| oag_core::Error::Config(format!("no principal with email {email}")))?
                .0;
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

const fn scope_of(owner_id: Option<Uuid>) -> &'static str {
    if owner_id.is_some() {
        "personal (bound to one principal)"
    } else {
        "shared pool"
    }
}

/// Seal the material and insert one account row, attached to a route.
#[allow(clippy::too_many_arguments)]
async fn insert_account(
    db: &Db,
    kek: &Kek,
    name: &str,
    provider: oag_core::Provider,
    kind: &str,
    material: &SecretMaterial,
    route: &str,
    max_concurrency: i32,
    priority: i16,
    owner_id: Option<Uuid>,
    monthly_cost: Option<Decimal>,
) -> Result<Uuid> {
    let sealed = kek.seal_json(material)?;
    // Denormalised so the scheduler can skip expired credentials without
    // decrypting every candidate; see the schema comment.
    let expires = material
        .expires_at
        .and_then(|e| time::OffsetDateTime::from_unix_timestamp(e).ok());

    let id = Uuid::now_v7();
    sqlx::query(
        r"
        INSERT INTO account (
            id, name, provider, kind, credentials_sealed, credentials_nonce,
            token_expires_at, owner_principal_id, priority, max_concurrency,
            monthly_cost_usd
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
        ",
    )
    .bind(id)
    .bind(name)
    .bind(provider.as_str())
    .bind(kind)
    .bind(&sealed.ciphertext)
    .bind(&sealed.nonce)
    .bind(expires)
    .bind(owner_id)
    .bind(priority)
    .bind(max_concurrency)
    .bind(monthly_cost)
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("creating account: {e}")))?;

    sqlx::query(
        "INSERT INTO account_route (account_id, route_id) SELECT $1, id FROM route WHERE name = $2",
    )
    .bind(id)
    .bind(route)
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("attaching account to route: {e}")))?;

    Ok(id)
}

async fn set_mode(db: &Db, route: &str, mode: &str) -> Result<()> {
    if !matches!(mode, "passthrough" | "managed") {
        return Err(oag_core::Error::Config(format!(
            "mode must be 'passthrough' or 'managed', not '{mode}'"
        )));
    }
    let n = sqlx::query("UPDATE route SET default_mode = $2, updated_at = now() WHERE name = $1")
        .bind(route)
        .bind(mode)
        .execute(db.pool())
        .await
        .map_err(|e| oag_core::Error::Internal(format!("setting mode: {e}")))?;
    if n.rows_affected() == 0 {
        return Err(oag_core::Error::Config(format!("no route named {route}")));
    }
    println!("route '{route}' mode: {mode}");
    if mode == "managed" {
        println!("  concrete model names will now be overridden by policy");
    } else {
        println!("  concrete model names will be honoured; oag/* stays managed");
    }
    Ok(())
}

async fn set_tiers_json(db: &Db, route: &str, tiers: &str) -> Result<()> {
    // Parse through the real type, so a malformed ladder is rejected here and
    // not on the first request that route serves.
    let rungs: Vec<oag_router::ladder::Rung> =
        serde_json::from_str(tiers).map_err(oag_core::Error::Serde)?;
    set_rungs(db, route, rungs).await
}

async fn seed_catalog(db: &Db, from: Option<&str>) -> Result<()> {
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

async fn sync_prices(db: &Db, kek: &Kek, provider: &str, account: Option<&str>) -> Result<()> {
    let known: oag_core::Provider = provider.parse()?;
    let row = price_account(db, known, account).await?;
    let material: SecretMaterial = kek.open_json(&row.sealed())?;

    let Some(prices) = oag_upstream::pricing::fetch(known, &material).await? else {
        return Err(oag_core::Error::Config(format!(
            "{known} publishes no price API; seed it from LiteLLM instead"
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

/// Pick the credential a price fetch authenticates with.
///
/// Any credential for the provider returns the same price list, so this takes
/// the first rather than making the operator name one; schedulable first,
/// because a disabled seat is usually disabled for a reason that will also stop
/// this call. There is no refresh here — the CLI has no `AppState` to hold the
/// fleet-wide lock — so a seat whose token has expired since the server last
/// touched it surfaces as a 401, and `--account` is the way past it.
async fn price_account(
    db: &Db,
    provider: oag_core::Provider,
    name: Option<&str>,
) -> Result<oag_store::AccountRow> {
    let row: Option<oag_store::AccountRow> = sqlx::query_as(
        r"
        SELECT id, name, provider, kind, credentials_sealed, credentials_nonce,
               token_version, token_expires_at, owner_principal_id, proxy_url,
               priority, max_concurrency, schedulable, cooldown_until,
               rate_limited_until, window_resets_at, last_used_at
        FROM account
        WHERE provider = $1 AND ($2::text IS NULL OR name = $2)
        ORDER BY schedulable DESC, priority DESC, name
        LIMIT 1
        ",
    )
    .bind(provider.as_str())
    .bind(name)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("finding a {provider} credential: {e}")))?;

    row.ok_or_else(|| {
        oag_core::Error::Config(match name {
            Some(n) => format!("no {provider} credential named {n}"),
            None => format!(
                "no {provider} credential; add one with `oag admin account add --provider {provider}`"
            ),
        })
    })
}

async fn revoke_key(db: &Db, redis_url: &str, prefix: &str) -> Result<()> {
    let Some((hash, name, prefix)) = repo::revoke_key_by_prefix(db, prefix).await? else {
        println!("no active key with prefix {prefix}");
        return Ok(());
    };

    // The row update alone is not a revocation: every replica caches auth by
    // hash, so without this the key keeps working until those entries expire.
    oag_store::Cache::connect(redis_url)?
        .auth_invalidate(&hash)
        .await;

    // Same target and shape as the server's audit line, so the CLI is not a
    // hole in the trail.
    tracing::warn!(
        target: "oag::audit",
        actor = "cli",
        action = "key.revoke",
        subject = %prefix,
        name,
        "admin write"
    );
    println!("revoked {name} ({prefix})");
    println!("  shared cache evicted; each replica's in-process cache expires within 15s");
    Ok(())
}

async fn flush_cache(redis_url: &str) -> Result<()> {
    let cache = oag_store::Cache::connect(redis_url)?;
    let n = cache.flush_auth_cache().await?;
    println!("dropped {n} cached auth entries");
    println!("  each replica's in-process cache expires within 15s");
    Ok(())
}

async fn status(db: &Db) -> Result<()> {
    let routes: Vec<(String, i64, Option<Decimal>)> = sqlx::query_as(
        r"
        SELECT r.name, count(ar.account_id), r.monthly_budget_usd
        FROM route r LEFT JOIN account_route ar ON ar.route_id = r.id
        GROUP BY r.id ORDER BY r.name
        ",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing routes: {e}")))?;

    println!("routes");
    for (name, accounts, budget) in routes {
        let b = budget.map_or_else(|| "uncapped".to_owned(), |b| format!("${b}/mo"));
        println!("  {name:<20} {accounts} credential(s)  {b}");
    }

    let accounts: Vec<(String, String, bool, Option<time::OffsetDateTime>)> = sqlx::query_as(
        "SELECT name, provider, schedulable, cooldown_until FROM account ORDER BY provider, name",
    )
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("listing accounts: {e}")))?;

    println!("\ncredentials");
    for (name, provider, schedulable, cooldown) in accounts {
        let state = if !schedulable {
            "disabled"
        } else if cooldown.is_some_and(|t| t > time::OffsetDateTime::now_utc()) {
            "cooling down"
        } else {
            "ready"
        };
        println!("  {name:<20} {provider:<12} {state}");
    }

    // The headline number: what the gateway saved this month.
    let spend: Option<(Decimal, Decimal, i64)> = sqlx::query_as(
        r"
        SELECT COALESCE(SUM(cost_usd),0), COALESCE(SUM(counterfactual_usd),0), COUNT(*)
        FROM usage_event WHERE occurred_at >= date_trunc('month', now())
        ",
    )
    .fetch_optional(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("summing spend: {e}")))?;

    if let Some((cost, counterfactual, n)) = spend {
        println!("\nthis month  {n} requests");
        println!("  spent            ${cost:.4}");
        println!("  frontier-for-all ${counterfactual:.4}");
        println!("  saved            ${:.4}", counterfactual - cost);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    #[command(name = "admin")]
    struct AdminCli {
        #[command(subcommand)]
        cmd: AdminCommand,
    }

    fn parse(args: &[&str]) -> std::result::Result<AdminCommand, clap::Error> {
        Ok(AdminCli::try_parse_from(std::iter::once("admin").chain(args.iter().copied()))?.cmd)
    }

    #[test]
    fn account_add_accepts_from_space_and_equals() {
        for args in [
            &["account", "add", "--name", "n", "--from", "grok"][..],
            &["account", "add", "--name", "n", "--from=codex"][..],
        ] {
            match parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}")) {
                AdminCommand::Account(AccountCommand::Add { args }) => {
                    assert!(args.from.is_some(), "{args:?}");
                }
                other => panic!("expected account add, got {other:?}"),
            }
        }
    }

    #[test]
    fn hidden_add_account_and_from_bools_still_parse() {
        match parse(&["add-account", "--name", "n", "--from-grok"])
            .unwrap_or_else(|e| panic!("{e}"))
        {
            AdminCommand::AddAccount { args } => assert!(args.from_grok),
            other => panic!("expected hidden add-account, got {other:?}"),
        }
    }

    #[test]
    fn key_create_and_legacy_key_flags_parse() {
        match parse(&["key", "create", "--email", "a@b.c"]).unwrap_or_else(|e| panic!("{e}")) {
            AdminCommand::Key(cli) => {
                assert!(matches!(cli.action, Some(KeyAction::Create { .. })));
            }
            other => panic!("expected key create, got {other:?}"),
        }
        match parse(&["key", "--email", "a@b.c"]).unwrap_or_else(|e| panic!("{e}")) {
            AdminCommand::Key(cli) => {
                assert!(cli.action.is_none());
                assert_eq!(cli.email.as_deref(), Some("a@b.c"));
            }
            other => panic!("expected legacy key, got {other:?}"),
        }
    }

    #[test]
    fn key_revoke_is_positional() {
        match parse(&["key", "revoke", "oag_live_abc"]).unwrap_or_else(|e| panic!("{e}")) {
            AdminCommand::Key(cli) => {
                assert!(matches!(
                    cli.action,
                    Some(KeyAction::Revoke { ref prefix }) if prefix == "oag_live_abc"
                ));
            }
            other => panic!("expected key revoke, got {other:?}"),
        }
    }

    #[test]
    fn route_tiers_parses_name_equals_models() {
        match parse(&[
            "route",
            "tiers",
            "cheap=xai/grok-4.3",
            "balanced=xai/grok-4.5",
        ])
        .unwrap_or_else(|e| panic!("{e}"))
        {
            AdminCommand::Route(RouteCommand::Tiers { rungs, .. }) => {
                assert_eq!(rungs.len(), 2);
                assert_eq!(rungs[0], "cheap=xai/grok-4.3");
            }
            other => panic!("expected route tiers, got {other:?}"),
        }
        let parsed = parse_ladder_rungs(&[
            "cheap=xai/grok-4.3,xai/grok-4".to_owned(),
            "balanced=xai/grok-4.5".to_owned(),
        ])
        .expect("rungs");
        assert_eq!(parsed[0].models.len(), 2);
        assert_eq!(parsed[1].name.as_str(), "balanced");
    }

    #[test]
    fn hidden_flat_spellings_still_parse() {
        assert!(matches!(
            parse(&["seed-catalog"]).unwrap_or_else(|e| panic!("{e}")),
            AdminCommand::SeedCatalog { .. }
        ));
        assert!(matches!(
            parse(&["flush-cache"]).unwrap_or_else(|e| panic!("{e}")),
            AdminCommand::FlushCache
        ));
        assert!(matches!(
            parse(&["revoke-key", "--prefix", "oag_live_x"]).unwrap_or_else(|e| panic!("{e}")),
            AdminCommand::RevokeKey { .. }
        ));
        assert!(matches!(
            parse(&["set-mode", "--mode", "managed"]).unwrap_or_else(|e| panic!("{e}")),
            AdminCommand::SetMode { .. }
        ));
    }
}
