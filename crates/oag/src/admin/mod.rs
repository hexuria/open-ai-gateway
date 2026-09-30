//! `oag admin` — the operations a human runs from a shell.
//!
//! Enough to stand up a working gateway without the UI existing yet, and to
//! recover one when the UI is the thing that is broken.
//!
//! Noun-verb grouping (`account add`, `key create`, `catalog seed`) is the
//! surface `--help` shows. The old flat spellings remain as hidden clap
//! aliases so existing scripts keep working without a deprecation line on
//! every CI call.

mod accounts;
mod catalog;
mod doctor;
mod endpoint_sync;
mod keys;
mod overview;
mod principals;
mod routes;
mod usage_import;

use accounts::{account_cmd, add_account_from_args};
use catalog::{catalog_cmd, print_providers, seed_catalog, sync_prices};
use clap::{Args, Subcommand, ValueEnum};
use keys::{key_cmd, revoke_key};
use oag_core::config::Config;
use oag_core::{Kek, Result};
use oag_store::Db;
use overview::{flush_cache, init, status};
use principals::promote_principal;
use routes::{route_cmd, set_mode, set_tiers_json};
use rust_decimal::Decimal;

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
    /// Operator-registered endpoints.
    #[command(subcommand)]
    Endpoint(EndpointCommand),
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
    /// Rename a credential. The way out of a duplicate name.
    Rename {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
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
    /// Bind a credential to the one person it belongs to.
    ///
    /// The way to fix a subscription seat left owner-less by an older
    /// version: until it is bound it serves no one (`oag admin doctor` lists
    /// such seats). It also moves a seat to a different owner.
    SetOwner {
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        owner_email: String,
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
    /// Force-release concurrency slots for a credential.
    ///
    /// Drops `oag:slots:{id}` in Redis. Use this when `oag_slots_in_use` is
    /// stuck at max and Redis has no live members — remasure bursts then 503
    /// `at_capacity` until something clears the view. The running replica's
    /// Prometheus gauge zeros on the next sweep (or immediately via the
    /// admin API).
    ClearSlots {
        #[arg(value_name = "NAME")]
        name: String,
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
    /// Parallel requests this credential may carry. Defaults to 2 for an
    /// imported seat — one person's CLI rarely has more in flight, and eight
    /// at once from one plan is a crowd — and to 8 for an API key.
    #[arg(long)]
    max_concurrency: Option<i32>,
    #[arg(long, default_value_t = 0)]
    priority: i16,
    /// The one person this credential belongs to. Required for a
    /// subscription seat (`--from`): a seat serves its owner and nobody else.
    /// Optional for an API key, which without it joins the organisation's
    /// shared pool. See docs/compliance.md.
    #[arg(long)]
    owner_email: Option<String>,
    /// Removed. A subscription seat cannot be pooled; kept hidden so a script
    /// that still passes it fails with the reason instead of "unknown flag".
    #[arg(long, hide = true)]
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

/// `oag admin endpoint`.
#[derive(Subcommand, Debug)]
pub enum EndpointCommand {
    /// `sync` and `models`: an endpoint's own model list.
    #[command(flatten)]
    Catalog(endpoint_sync::EndpointCatalogCommand),
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
        AdminCommand::Account(cmd) => account_cmd(db, kek, cmd, redis_url).await,
        AdminCommand::Key(cli) => key_cmd(db, redis_url, cli).await,
        AdminCommand::Route(cmd) => route_cmd(db, cmd).await,
        AdminCommand::Principal(PrincipalCommand::Promote { email }) => {
            promote_principal(db, &email).await
        }
        AdminCommand::Catalog(cmd) => catalog_cmd(db, kek, cmd).await,
        AdminCommand::Endpoint(EndpointCommand::Catalog(cmd)) => {
            endpoint_sync::run(db, kek, config, cmd).await
        }
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

#[cfg(test)]
mod tests;
