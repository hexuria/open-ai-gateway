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
mod usage_import;

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
    /// Principals: the identities keys are minted against.
    #[command(subcommand)]
    Principal(PrincipalCommand),
    /// Model catalog: seed, overlay prices, list.
    #[command(subcommand)]
    Catalog(CatalogCommand),
    /// The usage ledger: import traffic that bypassed the gateway.
    #[command(subcommand)]
    Usage(UsageCommand),
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

/// The one command that changes a principal's authority.
#[derive(Subcommand, Debug)]
pub enum PrincipalCommand {
    /// Grant the admin role.
    ///
    /// Deliberately separate from `init`, which used to grant it as a side
    /// effect of adding a route. There is no `demote`: see `promote_principal`.
    Promote {
        #[arg(long)]
        email: String,
    },
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
    /// Keep part of a subscription's quota back instead of spending it all.
    ///
    /// Without one, the gateway drains a seat until the provider answers 429 —
    /// at which point everybody on that seat is blocked until the window
    /// resets. A reserve stops scheduling it while there is still something
    /// left, so whoever needs it at the end of the week finds some.
    SetReserve {
        #[arg(value_name = "NAME")]
        name: String,
        /// The percentage to leave unspent, 0-100. Omit to clear the reserve,
        /// which restores the drain-to-empty behaviour.
        #[arg(long)]
        pct: Option<i16>,
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
    /// The provider API key.
    ///
    /// Falls back to `OAG_ACCOUNT_SECRET`, so it need not appear in shell
    /// history or the process table. Required with `--provider`, and read in
    /// `add_account_from_args` rather than declared here with clap's `env`.
    ///
    /// That is the whole of finding C8. clap treats an env-supplied value as
    /// explicitly present when it evaluates conflicts, so with
    /// `OAG_ACCOUNT_SECRET` exported — the documented way to keep a key out of
    /// shell history, recommended by this very help text — every `--from`
    /// import failed with "the argument '--secret' cannot be used with
    /// '--from'", naming a flag that was not on the command line. Only the
    /// operator who followed the advice could hit it.
    ///
    /// Reading the variable ourselves keeps the two cases distinguishable: a
    /// `--secret` that was typed conflicts with an importer and is refused
    /// below, and one inherited from the environment does not, because the
    /// operator was not asserting anything about this invocation.
    #[arg(long)]
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

/// The `origin` an imported Claude Code row carries.
///
/// Shared with the importer rather than spelled twice: it is the value the
/// idempotency key is prefixed with, the value `revert` deletes by, and the
/// value reporting slices on, so three copies would be three chances for a
/// typo to make an import unremovable.
pub(crate) const ORIGIN_CLAUDE_CODE: &str = "claude-code";

/// The `origin` an imported Grok CLI row carries. Its own value, not a shared
/// "imported": a Grok import is defended far less well than a Claude Code one,
/// so reverting the weaker of the two must not take the stronger with it.
pub(crate) const ORIGIN_GROK_CLI: &str = "grok-cli";

#[derive(Subcommand, Debug)]
pub enum UsageCommand {
    /// Fold local CLI session records into the ledger.
    ///
    /// Reads Claude Code's transcripts or the Grok CLI's session logs; pick
    /// with `--from`. Codex is not supported and is not omitted by oversight —
    /// it records no token counts of its own.
    ///
    /// The two sources are not equally well defended against re-importing
    /// traffic this gateway already metered. A Claude Code session is matched
    /// against the ledger call by call, and a transcript naming a non-Anthropic
    /// model proves it was proxied. Neither holds for the Grok CLI: it logs one
    /// aggregate per turn, which no per-call ledger row can equal, and a Grok
    /// model name says nothing about which endpoint answered. That source falls
    /// back to `--before` and to skipping any session this gateway was serving
    /// x.ai during. Each run prints which protections it actually applied.
    ///
    /// Reports and writes nothing unless `--apply` is given. Writing financial
    /// history into a ledger should take saying so.
    Import {
        /// Which CLI's records to read.
        #[arg(long, value_enum, default_value_t = usage_import::Source::ClaudeCode,
              value_name = "CLI")]
        from: usage_import::Source,
        /// Where the records are. A directory is walked for `*.jsonl`.
        /// Defaults to `~/.claude/projects`, or `~/.grok/sessions` for
        /// `--from grok-cli`.
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// Only import sessions that ended before this instant (RFC 3339).
        ///
        /// The one defence against double counting that does not depend on
        /// inference: set it to the moment you started routing this CLI through
        /// the gateway and no session it served can be imported, whatever the
        /// ledger does or does not still contain. For `--from grok-cli` it is
        /// the only exact protection there is.
        #[arg(long, value_name = "RFC3339")]
        before: Option<String>,
        /// The credential that paid for this usage, by name.
        ///
        /// A transcript records no account, so nothing on disk can say which
        /// subscription served it — you know, the file does not. Naming one
        /// attributes the rows to it, and if it is a subscription they book a
        /// marginal cost of zero and record the list price as the bill the
        /// monthly fee displaced, exactly as the gateway books a seat it serves
        /// itself. Without it the rows are booked as metered spend at list.
        #[arg(long, value_name = "NAME")]
        account: Option<String>,
        /// Write the rows.
        #[arg(long)]
        apply: bool,
    },
    /// Delete every row an importer wrote, leaving gateway traffic alone.
    Revert {
        /// Which import to undo: `claude-code` or `grok-cli`. One at a time,
        /// because they are not equally trustworthy and undoing the weaker
        /// should not cost the stronger.
        #[arg(long, default_value = ORIGIN_CLAUDE_CODE, value_name = "ORIGIN")]
        origin: String,
        /// Only rows attributed to this credential. Omit to remove the whole
        /// origin, which is what an operator undoing a first import wants and
        /// what naming an account deliberately does not do.
        #[arg(long, value_name = "NAME")]
        account: Option<String>,
        /// Actually delete them.
        #[arg(long)]
        apply: bool,
    },
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
        } => init(db, redis_url, &email, &route, budget_usd).await,
        AdminCommand::Status => status(db).await,
        AdminCommand::Doctor { route } => doctor::run(db, config, &route).await,
        AdminCommand::Providers => print_providers(db).await,
        AdminCommand::Account(cmd) => account_cmd(db, kek, cmd).await,
        AdminCommand::Key(cli) => key_cmd(db, redis_url, cli).await,
        AdminCommand::Route(cmd) => route_cmd(db, cmd).await,
        AdminCommand::Principal(PrincipalCommand::Promote { email }) => {
            promote_principal(db, &email).await
        }
        AdminCommand::Catalog(cmd) => catalog_cmd(db, kek, cmd).await,
        AdminCommand::Usage(cmd) => usage_cmd(db, cmd).await,
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

async fn usage_cmd(db: &Db, cmd: UsageCommand) -> Result<()> {
    match cmd {
        UsageCommand::Import {
            from,
            path,
            before,
            account,
            apply,
        } => usage_import::import(
            db,
            from,
            path.as_deref(),
            before.as_deref(),
            account.as_deref(),
            apply,
        )
        .await
        .map(|_| ()),
        UsageCommand::Revert {
            origin,
            account,
            apply,
        } => usage_import::revert(db, &origin, account.as_deref(), apply).await,
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
        AccountCommand::SetReserve { name, pct } => set_account_reserve(db, &name, pct).await,
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

/// Reject a reserve the column would reject anyway, before it costs a round
/// trip and comes back as a constraint violation.
///
/// The message names the range rather than merely saying "invalid", because
/// `--pct 0.15` is the mistake somebody makes once: a fraction where a
/// percentage was wanted parses as 0, and a reserve of 0 is a reserve that
/// never fires. Separated from the update so the range can be tested without a
/// database, which is the half of this that is worth testing.
fn validated_reserve(pct: Option<i16>) -> Result<Option<i16>> {
    match pct {
        Some(p) if !(0..=100).contains(&p) => Err(oag_core::Error::Config(format!(
            "reserve {p} is not a percentage; --pct takes 0-100"
        ))),
        other => Ok(other),
    }
}

/// Set or clear the share of a subscription's quota to leave unspent.
///
/// Accepts any account, on the same reasoning as `set_account_cost`: a reserve
/// on a metered key is inert — nothing polls it, so its remaining percentage
/// stays NULL and the scheduler never holds it back — and refusing one would
/// mean explaining the kinds taxonomy to somebody halfway through protecting a
/// seat.
async fn set_account_reserve(db: &Db, name: &str, pct: Option<i16>) -> Result<()> {
    let pct = validated_reserve(pct)?;
    let rows: Vec<(String, String, Option<rust_decimal::Decimal>)> = sqlx::query_as(
        "UPDATE account SET usage_reserve_pct = $2, updated_at = now() \
         WHERE name = $1 RETURNING name, kind, usage_remaining_pct",
    )
    .bind(name)
    .bind(pct)
    .fetch_all(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("updating account: {e}")))?;

    let Some((_, kind, remaining)) = rows.first() else {
        return Err(oag_core::Error::Config(format!(
            "no credential named {name}; see `oag admin account list`"
        )));
    };

    match pct {
        Some(p) => println!("{name} stops being scheduled at {p}% remaining"),
        None => println!("{name} has no reserve; it will be scheduled until the provider refuses"),
    }
    if kind != "oauth" {
        println!("  note: {name} is a {kind} credential, and only subscription");
        println!("  seats report how much of an allowance is left");
    } else if pct.is_some() && remaining.is_none() {
        // An unknown reading never holds a seat back, so a reserve set before
        // the first poll silently does nothing. Better said here than
        // discovered a week later when the seat is empty anyway.
        println!("  note: nothing has polled {name} yet, so the reserve holds");
        println!("  nothing back until a reading arrives");
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

    // The exclusion clap used to express, enforced where the distinction is
    // visible. An imported seat takes its credential from a signed-in CLI's
    // session file, so a `--secret` typed alongside `--from` would be silently
    // ignored — worth an error. One sitting in the environment is not: it is
    // there for every other invocation and says nothing about this one.
    if source.is_some() && secret.is_some() {
        return Err(oag_core::Error::Config(
            "--secret cannot be combined with --from: an imported seat takes its \
             credential from the CLI session file, so a secret passed here would be \
             ignored. A secret in OAG_ACCOUNT_SECRET is fine — this is only about the \
             flag."
                .to_owned(),
        ));
    }

    // Only after the conflict check, so the fallback cannot resurrect it.
    let secret = secret.or_else(|| std::env::var("OAG_ACCOUNT_SECRET").ok());
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
                    "--provider and --secret are required without --from. The secret may \
                     come from OAG_ACCOUNT_SECRET instead of the flag, which keeps it out \
                     of shell history."
                        .to_owned(),
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
            // The admin gate wants BOTH the key's flag and the principal's
            // role, so an admin key on a member principal is refused by every
            // admin endpoint it is presented to. Nothing checked, nothing
            // warned, and the troubleshooting doc sent the operator back to the
            // command that had just produced the unusable key.
            if admin {
                require_admin_principal(db, &email).await?;
            }
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
            if cli.admin {
                require_admin_principal(db, &email).await?;
            }
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
    Option<rust_decimal::Decimal>,
    Option<i16>,
);

async fn list_accounts(db: &Db) -> Result<()> {
    let rows: Vec<AccountListRow> = sqlx::query_as(
        r"
        SELECT name, provider, kind, schedulable, cooldown_until, rate_limited_until, priority,
               usage_remaining_pct, usage_reserve_pct
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
    println!("NAME                 PROVIDER     KIND       STATE          PRIORITY  RESERVE");
    let now = time::OffsetDateTime::now_utc();
    for (name, provider, kind, schedulable, cooldown, rate_limited, priority, remaining, reserve) in
        rows
    {
        // "held back" outranks "ready" and nothing else: a reserved-out seat is
        // as unschedulable as a rate limited one, and a listing that called it
        // ready would contradict the request that just failed on it.
        let state = if !schedulable {
            "disabled"
        } else if cooldown.is_some_and(|t| t > now) {
            "cooling down"
        } else if rate_limited.is_some_and(|t| t > now) {
            "rate limited"
        } else if reserve_holds(remaining, reserve) {
            "held back"
        } else {
            "ready"
        };
        // A dash rather than a blank where no reserve is set: a column that
        // simply stops has already been read as "the listing is truncated".
        let reserve = reserve.map_or_else(|| "-".to_owned(), |p| format!("{p}%"));
        println!("{name:<20} {provider:<12} {kind:<10} {state:<14} {priority:<9} {reserve}");
    }
    Ok(())
}

/// Whether a reserve is currently holding a credential out of the pool.
///
/// Borrowed from the scheduler rather than restated, so a listing can never
/// call a seat ready that the next request will refuse to use.
fn reserve_holds(remaining: Option<rust_decimal::Decimal>, reserve: Option<i16>) -> bool {
    oag_pool::held_by_reserve(remaining, reserve.map(rust_decimal::Decimal::from))
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

async fn init(
    db: &Db,
    redis_url: &str,
    email: &str,
    route: &str,
    budget: Option<Decimal>,
) -> Result<()> {
    let principal_id = upsert_principal(db, email, "admin", budget).await?;
    if budget.is_some() {
        evict_principal_keys(db, redis_url, principal_id, email).await;
    }
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
/// The role this principal holds, or `None` if there is no such principal.
/// Refuse to mint an admin key for a principal who is not an admin.
///
/// The gate is an AND of two facts and a key can only carry one of them. A key
/// minted with `--admin` against a member principal authenticates fine and is
/// refused by every admin endpoint, which reads as the admin API being broken
/// rather than as the key being half-privileged.
async fn require_admin_principal(db: &Db, email: &str) -> Result<()> {
    match principal_role(db, email).await?.as_deref() {
        Some("admin") => Ok(()),
        Some(role) => Err(oag_core::Error::Config(format!(
            "{email} is a {role}, so an --admin key minted for them would authenticate \
             and then be refused by every admin endpoint: the gate needs an admin key AND \
             an admin principal. Grant the role first with \
             `oag admin principal promote --email {email}`, or drop --admin for an \
             inference key."
        ))),
        // Left to `mint_key`, which names both lookups it could have been.
        None => Ok(()),
    }
}

async fn principal_role(db: &Db, email: &str) -> Result<Option<String>> {
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
async fn promote_principal(db: &Db, email: &str) -> Result<()> {
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
async fn evict_principal_keys(db: &Db, redis_url: &str, principal: Uuid, email: &str) {
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

    // `rows_affected`, because the SELECT is the whole statement's source: a
    // route name that does not match yields no rows, the INSERT writes nothing,
    // and `execute` calls that a success. The command then printed "attached to
    // route 'prod'" over a credential joined to nothing — schedulable, listed
    // as ready, and unreachable from any route, so every request through the
    // gateway failed `no_viable_model` while the CLI insisted the credential
    // was fine.
    //
    // The account row itself is left in place rather than rolled back. It holds
    // a sealed secret the operator has just supplied and may not have kept, and
    // destroying that to tidy up a typo is the worse of the two failures — the
    // message below says exactly what is missing and the fix is one command.
    let attached = sqlx::query(
        "INSERT INTO account_route (account_id, route_id) SELECT $1, id FROM route WHERE name = $2",
    )
    .bind(id)
    .bind(route)
    .execute(db.pool())
    .await
    .map_err(|e| oag_core::Error::Internal(format!("attaching account to route: {e}")))?;

    if attached.rows_affected() == 0 {
        return Err(oag_core::Error::Config(format!(
            "credential '{name}' was created but there is no route named '{route}', so it is \
             attached to nothing and no request can reach it. Create the route with \
             `oag admin init --route {route}`, then re-run this command; the credential \
             already stored is safe to delete or reuse."
        )));
    }

    Ok(id)
}

/// What `oag admin status` prints as this month's headline.
///
/// A constant so the statement can be run on its own in a test. The predicate
/// below is the whole of finding C4 and it is invisible from the outside: the
/// command prints a number, and a wrong number looks exactly like a right one.
///
/// Per-token traffic only, exactly as the admin API's headline does. A seat row
/// has `cost_usd` of zero and a real API-equivalent price, so folding it in lets
/// a flat-rate credential's zero marginal cost inflate the frontier saving —
/// the more a subscription is used, the better this line claims the gateway is
/// doing. Without it the two surfaces differed by an order of magnitude on any
/// deployment holding a seat, and an operator comparing `oag admin status` with
/// the dashboard had no way to tell which of them was lying.
const MONTH_HEADLINE_SQL: &str = r"
    SELECT COALESCE(SUM(cost_usd),0), COALESCE(SUM(counterfactual_usd),0),
           COUNT(*) FILTER (WHERE selection_reason NOT IN ('abandoned', 'lost'))
    FROM usage_event
    WHERE occurred_at >= date_trunc('month', now())
      AND NOT (cost_usd = 0 AND counterfactual_api_usd > 0)
";

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
    let revoked = repo::revoke_key_by_prefix(db, prefix).await?;
    if revoked.is_empty() {
        println!("no active key with prefix {prefix}");
        return Ok(());
    }

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
        println!("revoked {name} ({prefix})");
    }

    // Said loudly, because it means a key nobody asked about has just stopped
    // working. The prefix is displayed and not unique, so this is reachable
    // without anything being wrong with the database.
    if revoked.len() > 1 {
        println!(
            "\n  NOTE: {} keys shared the prefix {prefix} and all of them were revoked.",
            revoked.len()
        );
        println!("  If you meant only one, the others are named above and need re-issuing.");
    }
    // Said only when it happened. `auth_invalidate` used to swallow both an
    // unreachable Redis and a failed DEL, so this line printed either way — and
    // during a leaked-key incident it is the sentence the operator acts on. The
    // difference between the two outcomes is fifteen seconds and five minutes.
    if evicted {
        println!("  shared cache evicted; each replica's in-process cache expires within 15s");
    } else {
        println!();
        println!("  WARNING: the shared cache was NOT evicted — see the log above.");
        println!("  The key is inactive in the database but every replica will keep");
        println!("  accepting it from the cache for up to 5 minutes. Retry with");
        println!("  `oag admin cache flush` once the cache is reachable.");
    }
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
    let spend: Option<(Decimal, Decimal, i64)> = sqlx::query_as(MONTH_HEADLINE_SQL)
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

    /// C8. clap does not read `OAG_ACCOUNT_SECRET`, so it cannot conflict on it.
    ///
    /// clap treats an env-supplied value as explicitly present when it
    /// evaluates conflicts. With `env` on `--secret` and a `conflicts_with_all`
    /// against the importers, exporting the variable — the way this command's
    /// own help recommends keeping a key out of shell history — made every
    /// `oag admin account add --from codex` fail with "the argument '--secret'
    /// cannot be used with '--from'", naming a flag that was not on the command
    /// line. Only the operator who followed the advice could hit it.
    ///
    /// Asserted by introspecting the parser rather than by setting the variable,
    /// because the environment is process-global and mutating it from a test is
    /// `unsafe` — which this crate does not permit. Two facts make the bug
    /// unreachable, and both are checked: clap has no env binding for this
    /// argument, and no conflict declared against the importers.
    #[test]
    fn clap_neither_reads_the_secret_env_var_nor_conflicts_on_it() {
        use clap::CommandFactory as _;

        let cmd = AdminCli::command();
        let add = cmd
            .get_subcommands()
            .find(|c| c.get_name() == "account")
            .expect("account")
            .get_subcommands()
            .find(|c| c.get_name() == "add")
            .expect("add")
            .clone();
        let secret = add
            .get_arguments()
            .find(|a| a.get_id() == "secret")
            .expect("--secret");

        assert!(
            secret.get_env().is_none(),
            "an env binding here is what made the conflict fire on a variable; \
             the fallback is read in `add_account_from_args` instead"
        );
        // And the observable half: clap no longer refuses the combination at
        // all. The exclusion moved into `add_account_from_args`, where a typed
        // flag and an inherited variable can still be told apart — clap has no
        // public accessor for an argument's conflicts, so this is asserted by
        // parsing rather than by introspection.
        AdminCli::try_parse_from([
            "admin",
            "account",
            "add",
            "--name",
            "seat",
            "--from",
            "codex",
            "--secret",
            "typed-on-the-command-line",
        ])
        .expect("clap accepts it; the command is what refuses it");
    }

    /// And an importer parses without a secret, which is the invocation that broke.
    #[test]
    fn a_seat_import_parses_with_no_secret_flag() {
        let cli = AdminCli::try_parse_from([
            "admin",
            "account",
            "add",
            "--name",
            "codex-seat",
            "--from",
            "codex",
        ])
        .expect("an importer needs no secret");
        let AdminCommand::Account(AccountCommand::Add { args }) = cli.cmd else {
            panic!("expected an account add");
        };
        assert_eq!(args.from, Some(AccountSource::Codex));
        assert!(
            args.secret.is_none(),
            "the flag was not given, so the struct must not claim it was — the \
             environment is read later, after the conflict check"
        );
    }

    /// C5. Changing a budget at the CLI evicts the identities that cache it.
    ///
    /// A budget lives in the cached auth context, not only in the row. The HTTP
    /// path for this write has always evicted explicitly; `init` did not, so a
    /// lowered cap was unenforced on every replica for the cache's full five
    /// minutes, with nothing in the output hinting that a flush was needed. An
    /// operator who has just capped a runaway principal has every reason to
    /// believe they have capped them.
    #[tokio::test]
    async fn lowering_a_budget_at_the_cli_evicts_the_cached_identities() {
        let (Ok(url), Ok(redis_url)) = (
            std::env::var("OAG_TEST_DATABASE_URL"),
            std::env::var("OAG_TEST_REDIS_URL"),
        ) else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL or OAG_TEST_REDIS_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let email = format!("c5-{}@example.invalid", Uuid::new_v4());
        let route = format!("c5-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]')")
            .bind(&route)
            .execute(db.pool())
            .await
            .expect("route");
        let principal: Uuid = sqlx::query_scalar(
            "INSERT INTO principal (id, email, role, monthly_budget_usd)
             VALUES (gen_random_uuid(), $1, 'member', 100) RETURNING id",
        )
        .bind(&email)
        .fetch_one(db.pool())
        .await
        .expect("principal");
        let key = mint_key(&db, &email, &route, "c5", None, false)
            .await
            .expect("mint");
        let hash = repo::hash_key(&key);

        // An identity in the shared cache, as a live request would leave.
        let cache = oag_store::Cache::connect(&redis_url).expect("cache");
        let mac = oag_store::AuthMac::new("test-signing-secret-for-c5-eviction-0001");
        let ctx = oag_store::AuthContext {
            api_key_id: Uuid::new_v4(),
            principal_id: principal,
            route_id: Uuid::new_v4(),
            key_floor_tier: None,
            admin: false,
            quota_usd: None,
            principal_budget_usd: Some(Decimal::from(100)),
            principal_hard_stop_multiple: Decimal::from(2),
            key_hash: hash.clone(),
        };
        cache
            .auth_set(&hash, &ctx, std::time::Duration::from_secs(300), &mac)
            .await;
        assert!(
            cache.auth_get(&hash, &mac).await.is_some(),
            "the fixture has to be cached for the eviction to mean anything"
        );

        evict_principal_keys(&db, &redis_url, principal, &email).await;

        assert!(
            cache.auth_get(&hash, &mac).await.is_none(),
            "the new cap is not enforced until this entry is gone, and five \
             minutes of an uncapped principal is the whole finding"
        );
    }

    /// C3. A credential attached to nothing is an error, not a success line.
    ///
    /// The `account_route` insert selects from `route`, so a name that does not
    /// match yields no rows and the INSERT writes nothing — which `execute`
    /// reports as success. The command then printed "attached to route 'prod'"
    /// over a credential joined to nothing: schedulable, listed as ready,
    /// unreachable from any route, and every request through the gateway
    /// failing `no_viable_model` while the CLI insisted the credential was fine.
    #[tokio::test]
    async fn adding_a_credential_to_a_missing_route_is_an_error() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");
        let kek = oag_core::Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=")
            .expect("kek");

        let name = format!("c3-{}", Uuid::new_v4());
        let err = add_account(
            &db,
            &kek,
            &name,
            "anthropic",
            "not-a-real-secret-for-tests",
            "no-such-route",
            4,
            0,
            None,
            None,
        )
        .await
        .expect_err("no route, so nothing to attach to");
        let message = err.to_string();
        assert!(
            message.contains("no-such-route") && message.contains(&name),
            "the operator needs both halves to act on it: {message}"
        );

        // The credential itself survives: it holds a secret the operator has
        // just supplied and may not have kept, and destroying that to tidy up a
        // typo is the worse failure.
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE name = $1")
            .bind(&name)
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(stored, 1, "the sealed secret is not thrown away");

        // And it is joined to nothing, which is what the error said.
        let joins: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM account_route ar
               JOIN account a ON a.id = ar.account_id WHERE a.name = $1",
        )
        .bind(&name)
        .fetch_one(db.pool())
        .await
        .expect("count");
        assert_eq!(joins, 0);
    }

    /// C6. Adding a route does not hand out the admin role.
    ///
    /// `init`'s upsert set `role = EXCLUDED.role` with the role hard-coded to
    /// `admin`, so `oag admin init --email someone@corp.com --route staging` —
    /// a command whose stated job is adding a route — silently promoted whoever
    /// that email named and then minted them an admin key. Nothing in the
    /// output mentioned a role, because from the command's point of view
    /// nothing had changed: it asked for an admin and got one.
    ///
    /// The store's own `upsert_principal` has always omitted `role` here, for
    /// the mirror-image reason: an idempotent bind must not be able to *remove*
    /// authority either.
    #[tokio::test]
    async fn init_against_an_existing_principal_leaves_their_role_alone() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let email = format!("c6-{}@example.invalid", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO principal (id, email, role, monthly_budget_usd)
             VALUES (gen_random_uuid(), $1, 'member', 50)",
        )
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("seed a member");

        // The call `init` makes, asking for admin as it always has.
        upsert_principal(&db, &email, "admin", None)
            .await
            .expect("upsert");
        assert_eq!(
            principal_role(&db, &email).await.expect("role").as_deref(),
            Some("member"),
            "adding a route is not a grant of authority"
        );

        // The budget is still protected from an init that omits it.
        let budget: Option<Decimal> =
            sqlx::query_scalar("SELECT monthly_budget_usd FROM principal WHERE email = $1")
                .bind(&email)
                .fetch_one(db.pool())
                .await
                .expect("budget");
        assert_eq!(budget, Some(Decimal::from(50)), "COALESCE still guards it");

        // And a principal that does not exist yet is still created as asked —
        // promoting the first admin is what `init` is for.
        let fresh = format!("c6-first-{}@example.invalid", Uuid::new_v4());
        upsert_principal(&db, &fresh, "admin", None)
            .await
            .expect("upsert");
        assert_eq!(
            principal_role(&db, &fresh).await.expect("role").as_deref(),
            Some("admin")
        );

        // Granting is its own command, and it is idempotent.
        promote_principal(&db, &email).await.expect("promote");
        assert_eq!(
            principal_role(&db, &email).await.expect("role").as_deref(),
            Some("admin")
        );
        promote_principal(&db, &email).await.expect("promote again");
    }

    /// C7. An `--admin` key is refused for a principal who is not an admin.
    ///
    /// The admin gate is an AND of two facts — the key's flag and the
    /// principal's role — and a key can only carry one of them. Minted against
    /// a member, an admin key authenticates fine and is then refused by every
    /// admin endpoint, which reads as the admin API being broken rather than as
    /// the key being half-privileged. Nothing checked it, nothing warned, and
    /// the CLI had no command that could set a role.
    #[tokio::test]
    async fn an_admin_key_is_refused_for_a_principal_who_is_not_one() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let email = format!("c7-{}@example.invalid", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')",
        )
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("seed a member");

        let err = require_admin_principal(&db, &email)
            .await
            .expect_err("a member cannot hold an admin key");
        let message = err.to_string();
        assert!(
            message.contains("oag admin principal promote"),
            "the operator needs the command that fixes it, not just the refusal: {message}"
        );

        // An inference key for the same principal is unaffected: only the
        // combination is refused.
        promote_principal(&db, &email).await.expect("promote");
        require_admin_principal(&db, &email)
            .await
            .expect("an admin may hold an admin key");
    }

    /// H9. A key that was not stored is not printed.
    ///
    /// The INSERT selects from `principal` and `route`, so it inserts nothing
    /// when either lookup misses — and `execute` reports that as a successful
    /// statement affecting zero rows. The plaintext was printed anyway, under
    /// "This is shown once", which was true in the worst possible way: never
    /// stored, so unrecoverable and unable to ever authenticate.
    ///
    /// The developer holding it gets 401 on every request, `key list` shows
    /// nothing, and the incident reads as broken auth rather than as a mistyped
    /// route name.
    #[tokio::test]
    async fn minting_against_a_missing_route_or_principal_is_an_error_not_a_key() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        // A route that exists, so only the principal is missing.
        let route = format!("h9-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO route (id, name, tiers) VALUES (gen_random_uuid(), $1, '[]')")
            .bind(&route)
            .execute(db.pool())
            .await
            .expect("seed route");

        let missing_principal = format!("nobody-{}@example.invalid", Uuid::new_v4());
        let err = mint_key(&db, &missing_principal, &route, "k", None, false)
            .await
            .expect_err("no principal, so no key");
        let message = err.to_string();
        assert!(
            message.contains(&missing_principal) && message.contains(&route),
            "the operator cannot see which lookup missed, so both are named: {message}"
        );

        // And nothing was written under either name.
        let keys: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM api_key k JOIN route r ON r.id = k.route_id WHERE r.name = $1",
        )
        .bind(&route)
        .fetch_one(db.pool())
        .await
        .expect("count");
        assert_eq!(keys, 0, "a failed mint leaves no row");

        // A principal that exists and a route that does not: the same answer,
        // because the SELECT is a cross join and either side empties it.
        let email = format!("h9-{}@example.invalid", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO principal (id, email, role) VALUES (gen_random_uuid(), $1, 'member')",
        )
        .bind(&email)
        .execute(db.pool())
        .await
        .expect("seed principal");
        mint_key(&db, &email, "no-such-route-here", "k", None, false)
            .await
            .expect_err("no route, so no key");

        // Both present: a key, and it is really there.
        let key = mint_key(&db, &email, &route, "k", None, false)
            .await
            .expect("both exist");
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM api_key WHERE key_hash = $1")
            .bind(repo::hash_key(&key))
            .fetch_one(db.pool())
            .await
            .expect("count");
        assert_eq!(
            stored, 1,
            "the key that was printed is the key that was stored"
        );
    }

    /// C4. The CLI headline counts per-token traffic only, as the API does.
    ///
    /// A seat row has `cost_usd` of zero and a real API-equivalent price. The
    /// admin API filters those out of the headline deliberately — a flat-rate
    /// credential's zero marginal cost would otherwise inflate the frontier
    /// saving, so the more a subscription was used the better the gateway would
    /// claim to be doing. The CLI did not filter, so on any deployment holding
    /// a seat the two surfaces differed by an order of magnitude and an
    /// operator comparing them had no way to tell which was lying.
    ///
    /// The statement is run directly rather than through `status`, which
    /// prints: the number is the finding, and a wrong number looks exactly like
    /// a right one on a terminal.
    #[tokio::test]
    async fn the_month_headline_leaves_seat_rows_out() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        let before: (Decimal, Decimal, i64) = sqlx::query_as(MONTH_HEADLINE_SQL)
            .fetch_one(db.pool())
            .await
            .expect("headline");

        // One metered row and one seat row, this month, for the same tokens.
        // The seat's cost is truthfully zero and its displaced API bill is
        // large — which is exactly what makes it poison for a saving figure.
        for (cost, api) in [("2.50", "2.50"), ("0", "40.00")] {
            sqlx::query(
                "INSERT INTO usage_event (request_id, attempt, model_id, tier, \
                 selection_reason, input_tokens, output_tokens, cost_usd, \
                 counterfactual_usd, counterfactual_api_usd, status) \
                 VALUES ($1, 0, 'anthropic/claude-opus-5', 'frontier', 'classified', \
                         100, 20, $2::numeric, 9.00, $3::numeric, 200)",
            )
            .bind(Uuid::new_v4())
            .bind(cost)
            .bind(api)
            .execute(db.pool())
            .await
            .expect("seed");
        }

        let after: (Decimal, Decimal, i64) = sqlx::query_as(MONTH_HEADLINE_SQL)
            .fetch_one(db.pool())
            .await
            .expect("headline");

        assert_eq!(
            after.2 - before.2,
            1,
            "two rows landed and exactly one of them is per-token traffic"
        );
        assert_eq!(
            after.0 - before.0,
            Decimal::from_str_exact("2.50").expect("decimal"),
            "the seat contributed no spend"
        );
        assert_eq!(
            after.1 - before.1,
            Decimal::from_str_exact("9.00").expect("decimal"),
            "and no counterfactual — its zero cost against a frontier baseline \
             is the free saving that made this figure a lie"
        );
    }

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
    fn set_reserve_parses_a_percentage_and_a_clear() {
        match parse(&["account", "set-reserve", "grok", "--pct", "15"])
            .unwrap_or_else(|e| panic!("{e}"))
        {
            AdminCommand::Account(AccountCommand::SetReserve { name, pct }) => {
                assert_eq!(name, "grok");
                assert_eq!(pct, Some(15));
            }
            other => panic!("expected account set-reserve, got {other:?}"),
        }
        // Omitting `--pct` is how a reserve is removed, exactly as omitting
        // `--monthly-cost` clears a price.
        match parse(&["account", "set-reserve", "grok"]).unwrap_or_else(|e| panic!("{e}")) {
            AdminCommand::Account(AccountCommand::SetReserve { pct, .. }) => assert_eq!(pct, None),
            other => panic!("expected account set-reserve, got {other:?}"),
        }
    }

    #[test]
    fn a_reserve_outside_the_percentage_range_is_refused_by_naming_the_range() {
        // `--pct 150` and `--pct -1` are typos, and a database CHECK violation
        // is not an answer anybody can act on. The range has to be in the text
        // or the next guess is as blind as the first.
        for bad in [-1, 101, 1_000] {
            let e = validated_reserve(Some(bad)).expect_err("out of range");
            let message = e.to_string();
            assert!(message.contains("0-100"), "{message}");
            assert!(message.contains(&bad.to_string()), "{message}");
        }
    }

    #[test]
    fn the_ends_of_the_range_and_a_cleared_reserve_are_accepted() {
        // 100 is "never schedule this seat", which is a legitimate, if blunt,
        // way to park one; 0 is a reserve that only fires on a truly empty
        // pool; None is no reserve at all.
        assert_eq!(
            validated_reserve(Some(0)).unwrap_or_else(|e| panic!("{e}")),
            Some(0)
        );
        assert_eq!(
            validated_reserve(Some(100)).unwrap_or_else(|e| panic!("{e}")),
            Some(100)
        );
        assert_eq!(
            validated_reserve(None).unwrap_or_else(|e| panic!("{e}")),
            None
        );
    }

    #[test]
    fn a_listing_calls_a_seat_held_back_only_when_the_scheduler_would_refuse_it() {
        use rust_decimal::Decimal;
        assert!(reserve_holds(Some(Decimal::from(5)), Some(10)));
        assert!(
            reserve_holds(Some(Decimal::from(10)), Some(10)),
            "at the line"
        );
        assert!(!reserve_holds(Some(Decimal::from(45)), Some(10)));
        // Unknown is not empty, and no reserve is no policy.
        assert!(!reserve_holds(None, Some(10)));
        assert!(!reserve_holds(Some(Decimal::ZERO), None));
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
