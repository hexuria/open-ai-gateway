//! `oag admin endpoint sync` and `oag admin endpoint models`: an endpoint's
//! own model list, read with one of its keys. `sync` writes the chat models
//! the list prices into the catalog; `models` shows the list as discovery
//! reads it, beside the catalog, and writes nothing.
//!
//! The work is `oag_server::endpoint_sync`'s. What is here is the command line
//! and what it prints.

use clap::{Subcommand, ValueEnum};
use oag_core::config::Config;
use oag_core::{Kek, Result};
use oag_server::endpoint_sync::{self, ModelsReport, SyncOptions, SyncReport};
use oag_store::Db;
use oag_upstream::listing::PriceChoice;
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum EndpointCatalogCommand {
    /// Write an endpoint's priced model list into the catalog.
    ///
    /// Reads the list with one of the endpoint's keys: at --listing-url if
    /// given, else at `{base}/models` (`{base}/v1/models` for an anthropic
    /// endpoint), else at `/v1/models` on the base URL's host, which is where
    /// Merge Gateway keeps it. Only a list that prices each model per vendor is
    /// read; an id-only list is refused, because a catalog row needs a price.
    ///
    /// Each chat model the list prices becomes, or rewrites, the row
    /// `<endpoint>/<model>`, as an override, with a label where the row has
    /// none. A model the list no longer offers is removed, unless a route's
    /// ladder names it. The running gateway serves the rows from its next
    /// catalog refresh.
    Sync {
        #[arg(value_name = "NAME")]
        name: String,
        /// The credential to read the list with. Defaults to the endpoint's
        /// first schedulable one.
        #[arg(long)]
        account: Option<String>,
        /// Report what would change, and write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Manage only the models this glob matches, by upstream id
        /// (`zai/*`) or catalog id (`merge/zai/*`); `*` is any run of
        /// characters, `?` any one. Repeatable. The rest are neither written
        /// nor removed.
        #[arg(long, value_name = "GLOB")]
        include: Vec<String>,
        /// Leave alone the models this glob matches. Repeatable; wins over
        /// --include.
        #[arg(long, value_name = "GLOB")]
        exclude: Vec<String>,
        /// Which vendor prices a model more than one serves.
        #[arg(long, value_enum, default_value_t = Price::Cheapest)]
        price: Price,
        /// Where the priced list is, when it is not where the sync looks. Must
        /// be on the endpoint's own host, since its key goes with the request.
        /// Used for this run only; nothing stores it.
        #[arg(long, value_name = "URL")]
        listing_url: Option<String>,
    },
    /// Show the models an endpoint's list names, as discovery reads it, beside
    /// the catalog. Writes nothing.
    Models {
        #[arg(value_name = "NAME")]
        name: String,
        /// The credential to read the list with. Defaults to the endpoint's
        /// first schedulable one.
        #[arg(long)]
        account: Option<String>,
    },
}

/// `--price`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Price {
    /// The cheapest vendor that can serve the model: where Merge sends it.
    Cheapest,
    /// The first vendor listed that can serve it.
    First,
}

impl From<Price> for PriceChoice {
    fn from(price: Price) -> Self {
        match price {
            Price::Cheapest => Self::Cheapest,
            Price::First => Self::First,
        }
    }
}

pub(super) async fn run(
    db: &Db,
    kek: &Kek,
    config: &Config,
    cmd: EndpointCatalogCommand,
) -> Result<()> {
    let lines = match cmd {
        EndpointCatalogCommand::Sync {
            name,
            account,
            dry_run,
            include,
            exclude,
            price,
            listing_url,
        } => {
            let options = SyncOptions {
                account,
                listing_url,
                include,
                exclude,
                price: price.into(),
                dry_run,
            };
            let report = endpoint_sync::sync(db, kek, &name, &options).await?;
            sync_lines(&report, config.gateway.catalog_refresh_interval)
        }
        EndpointCatalogCommand::Models { name, account } => {
            models_lines(&endpoint_sync::models(db, kek, &name, account.as_deref()).await?)
        }
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// How many ids a count shows before it says how many more there are.
const SHOWN: usize = 5;

/// Up to [`SHOWN`] of `ids`, and how many it left out.
fn some_of<S: AsRef<str>>(ids: &[S]) -> String {
    let shown: Vec<&str> = ids.iter().take(SHOWN).map(AsRef::as_ref).collect();
    match ids.len().saturating_sub(SHOWN) {
        0 => shown.join(", "),
        more => format!("{}, and {more} more", shown.join(", ")),
    }
}

/// What `endpoint sync` prints. Every count is shown, zero included, so a
/// summary always reads the same way; removed and kept rows are named in full,
/// because each is a row the operator may want back.
pub(super) fn sync_lines(report: &SyncReport, refresh: Duration) -> Vec<String> {
    let would = |done: &'static str, planned: &'static str| {
        if report.dry_run { planned } else { done }
    };
    let credential = report
        .account
        .as_deref()
        .map_or_else(|| "no credential".to_owned(), |a| format!("credential {a}"));
    let pages = if report.pages == 1 { "page" } else { "pages" };
    let mut lines = vec![format!(
        "{}{}: read {} ({} {pages}) with {credential}; priced by the {} vendor",
        if report.dry_run { "dry run, " } else { "" },
        report.endpoint,
        report.url,
        report.pages,
        report.price.as_str(),
    )];
    let count = |label: &str, ids: &[String], all: bool| {
        let named = if all { ids.join(", ") } else { some_of(ids) };
        format!("  {label:<14} {:>4}  {named}", ids.len())
            .trim_end()
            .to_owned()
    };
    lines.push(count(would("added", "would add"), &report.added, false));
    lines.push(count(
        would("updated", "would update"),
        &report.updated,
        false,
    ));
    lines.push(count("unchanged", &report.unchanged, false));
    lines.push(count(
        would("removed", "would remove"),
        &report.removed,
        true,
    ));
    lines.push(count("kept (ladder)", &report.kept_on_ladder, true));
    if !report.kept_on_ladder.is_empty() {
        lines.push(
            "                 no longer offered by the list, and kept because a route's \
             ladder names them"
                .to_owned(),
        );
    }
    if !report.held.is_empty() {
        lines.push(count("held", &report.held, true));
        lines.push(
            "                 another provider's row already has the id; left as it was".to_owned(),
        );
    }
    lines.push(format!("  {:<14} {:>4}", "skipped", report.skipped.len()));
    // Grouped by reason, in the order each reason first appears.
    let mut reasons: Vec<(&str, Vec<&str>)> = Vec::new();
    for (name, why) in &report.skipped {
        match reasons.iter_mut().find(|(label, _)| *label == why.label()) {
            Some((_, names)) => names.push(name),
            None => reasons.push((why.label(), vec![name])),
        }
    }
    for (label, names) in reasons {
        lines.push(format!(
            "    {label}: {} ({})",
            names.len(),
            some_of(&names)
        ));
    }
    lines.push(count("filtered out", &report.filtered, false));
    lines.push(if report.dry_run {
        "Dry run: nothing was written. Run it again without --dry-run to write it.".to_owned()
    } else if refresh.is_zero() {
        "Catalog refresh is off here (catalog_refresh_interval: 0): reload it with \
         POST /admin/api/catalog/reload, or restart the gateway."
            .to_owned()
    } else {
        format!(
            "The running gateway serves these rows from its next catalog refresh, within \
             {}s; no restart is needed.",
            refresh.as_secs()
        )
    });
    lines
}

/// What `endpoint models` prints.
pub(super) fn models_lines(report: &ModelsReport) -> Vec<String> {
    let credential = report
        .account
        .as_deref()
        .map_or_else(|| "no credential".to_owned(), |a| format!("credential {a}"));
    let pages = if report.pages == 1 { "page" } else { "pages" };
    let discovery = if report.discover {
        "discovery is on: the usage poller records this list for each of its keys"
    } else {
        "discovery is off: nothing records this list"
    };
    let mut lines = vec![format!(
        "{}: {} models listed at {} ({} {pages}), read with {credential}; {discovery}",
        report.endpoint,
        report.listed.len(),
        report.url,
        report.pages,
    )];
    let width = report
        .listed
        .iter()
        .map(|(id, _)| id.len())
        .max()
        .unwrap_or(0);
    for (id, catalog) in &report.listed {
        let catalog = catalog.as_deref().unwrap_or("(not in the catalog)");
        lines.push(format!("  {id:<width$}  {catalog}"));
    }
    if !report.unlisted.is_empty() {
        lines.push(format!(
            "{} catalog rows the list does not name{}:",
            report.unlisted.len(),
            if report.discover {
                ", which discovery hides"
            } else {
                ", which discovery would hide"
            }
        ));
        for id in &report.unlisted {
            lines.push(format!("  {id}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{AdminCommand, EndpointCommand};
    use clap::Parser;
    use oag_upstream::listing::Skip;

    #[derive(Parser, Debug)]
    #[command(name = "admin")]
    struct AdminCli {
        #[command(subcommand)]
        cmd: AdminCommand,
    }

    fn parse(args: &[&str]) -> std::result::Result<EndpointCatalogCommand, clap::Error> {
        let cli = AdminCli::try_parse_from(std::iter::once("admin").chain(args.iter().copied()))?;
        let AdminCommand::Endpoint(EndpointCommand::Catalog(cmd)) = cli.cmd else {
            panic!("expected an endpoint command");
        };
        Ok(cmd)
    }

    #[test]
    fn sync_takes_every_flag_and_defaults_to_the_cheapest_vendor() {
        let EndpointCatalogCommand::Sync {
            name,
            account,
            dry_run,
            include,
            exclude,
            price,
            listing_url,
        } = parse(&["endpoint", "sync", "merge"]).expect("parses")
        else {
            panic!("expected sync");
        };
        assert_eq!(name, "merge");
        assert_eq!((account, dry_run, listing_url), (None, false, None));
        assert!(include.is_empty() && exclude.is_empty());
        assert_eq!(price, Price::Cheapest);
        assert_eq!(PriceChoice::from(price), PriceChoice::Cheapest);

        let EndpointCatalogCommand::Sync {
            account,
            dry_run,
            include,
            exclude,
            price,
            listing_url,
            ..
        } = parse(&[
            "endpoint",
            "sync",
            "merge",
            "--account",
            "merge-key",
            "--dry-run",
            "--include",
            "zai/*",
            "--include",
            "anthropic/*",
            "--exclude",
            "*-preview",
            "--price",
            "first",
            "--listing-url",
            "https://api-gateway.merge.dev/v1/models",
        ])
        .expect("parses")
        else {
            panic!("expected sync");
        };
        assert_eq!(account.as_deref(), Some("merge-key"));
        assert!(dry_run);
        assert_eq!(include, ["zai/*", "anthropic/*"]);
        assert_eq!(exclude, ["*-preview"]);
        assert_eq!(PriceChoice::from(price), PriceChoice::First);
        assert_eq!(
            listing_url.as_deref(),
            Some("https://api-gateway.merge.dev/v1/models")
        );

        assert!(
            parse(&["endpoint", "sync"]).is_err(),
            "the endpoint is named"
        );
        assert!(
            parse(&["endpoint", "sync", "merge", "--price", "dearest"]).is_err(),
            "cheapest or first"
        );
    }

    #[test]
    fn models_takes_a_name_and_an_optional_credential() {
        let EndpointCatalogCommand::Models { name, account } =
            parse(&["endpoint", "models", "merge", "--account", "k"]).expect("parses")
        else {
            panic!("expected models");
        };
        assert_eq!((name.as_str(), account.as_deref()), ("merge", Some("k")));
    }

    fn report(dry_run: bool) -> SyncReport {
        SyncReport {
            endpoint: "merge".to_owned(),
            url: "https://api-gateway.merge.dev/v1/models?limit=500".to_owned(),
            pages: 2,
            account: Some("merge-key".to_owned()),
            price: PriceChoice::Cheapest,
            dry_run,
            added: (1..=7).map(|n| format!("merge/v/m-{n}")).collect(),
            updated: vec!["merge/zai/glm-5.3-flash".to_owned()],
            unchanged: vec!["merge/anthropic/claude-sonnet-4.5".to_owned()],
            removed: vec!["merge/old/a".to_owned(), "merge/old/b".to_owned()],
            kept_on_ladder: vec!["merge/old/c".to_owned()],
            held: Vec::new(),
            skipped: vec![
                (
                    "openai/gpt-image-1".to_owned(),
                    Skip::NotChat("image".to_owned()),
                ),
                (
                    "mistral/large".to_owned(),
                    Skip::Unavailable("deprecated".to_owned()),
                ),
                ("google/veo-3".to_owned(), Skip::NotChat("video".to_owned())),
            ],
            filtered: Vec::new(),
        }
    }

    #[test]
    fn a_sync_summary_counts_everything_and_says_when_the_gateway_sees_it() {
        let lines = sync_lines(&report(false), Duration::from_mins(1));
        let text = lines.join("\n");
        assert!(
            lines[0].starts_with("merge: read https://api-gateway.merge.dev/v1/models?limit=500 (2 pages) with credential merge-key; priced by the cheapest vendor"),
            "{text}"
        );
        for expected in [
            "  added             7  merge/v/m-1, merge/v/m-2, merge/v/m-3, merge/v/m-4, merge/v/m-5, and 2 more",
            "  updated           1  merge/zai/glm-5.3-flash",
            "  unchanged         1  merge/anthropic/claude-sonnet-4.5",
            "  removed           2  merge/old/a, merge/old/b",
            "  kept (ladder)     1  merge/old/c",
            "                 no longer offered by the list, and kept because a route's ladder \
             names them",
            "  skipped           3",
            "    not a chat model: 2 (openai/gpt-image-1, google/veo-3)",
            "    deprecated or unavailable: 1 (mistral/large)",
            "  filtered out      0",
        ] {
            assert!(
                lines.iter().any(|l| l == expected),
                "{expected:?} in\n{text}"
            );
        }
        assert!(
            text.contains("next catalog refresh, within 60s; no restart is needed"),
            "{text}"
        );
        assert!(!text.contains("held"), "nothing held, nothing said: {text}");
        let mut none_kept = report(false);
        none_kept.kept_on_ladder.clear();
        let none_kept = sync_lines(&none_kept, Duration::from_mins(1)).join("\n");
        assert!(
            !none_kept.contains("no longer offered"),
            "nothing kept, nothing explained: {none_kept}"
        );

        let off = sync_lines(&report(false), Duration::ZERO).join("\n");
        assert!(off.contains("catalog_refresh_interval: 0"), "{off}");
    }

    #[test]
    fn a_dry_run_summary_says_what_would_change_and_that_nothing_did() {
        let lines = sync_lines(&report(true), Duration::from_mins(1));
        let text = lines.join("\n");
        assert!(lines[0].starts_with("dry run, merge: read "), "{text}");
        assert!(
            lines.iter().any(|l| l.starts_with("  would add         7")),
            "{text}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "  would remove      2  merge/old/a, merge/old/b"),
            "{text}"
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some("Dry run: nothing was written. Run it again without --dry-run to write it.")
        );
    }

    #[test]
    fn the_models_listing_marks_what_the_catalog_serves_and_what_it_would_hide() {
        let lines = models_lines(&ModelsReport {
            endpoint: "merge".to_owned(),
            discover: false,
            account: Some("merge-key".to_owned()),
            url: "https://api-gateway.merge.dev/v1/openai/models".to_owned(),
            pages: 1,
            listed: vec![
                (
                    "zai/glm-5.3-flash".to_owned(),
                    Some("merge/zai/glm-5.3-flash".to_owned()),
                ),
                ("zai/new".to_owned(), None),
            ],
            unlisted: vec!["merge/old/a".to_owned()],
        });
        assert_eq!(
            lines,
            [
                "merge: 2 models listed at https://api-gateway.merge.dev/v1/openai/models \
                 (1 page), read with credential merge-key; discovery is off: nothing \
                 records this list",
                "  zai/glm-5.3-flash  merge/zai/glm-5.3-flash",
                "  zai/new            (not in the catalog)",
                "1 catalog rows the list does not name, which discovery would hide:",
                "  merge/old/a",
            ]
        );
    }

    /// Both dispatchers, `admin::run` and this module's, hand back what the
    /// command said. With a pool that cannot connect, the command fails before
    /// any request is sent; a dispatcher that answered `Ok` without running it
    /// would pass for a working one.
    #[tokio::test]
    async fn the_dispatchers_surface_the_commands_error() {
        let db = Db::connect("postgres://oag:oag@127.0.0.1:1/oag_g0", 1).expect("lazy pool");
        let kek = Kek::from_base64("b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=").expect("kek");
        let config = oag_core::config::Config::from_yaml(
            "database:\n  url: \"postgres://oag:oag@127.0.0.1:1/oag_g0\"\nredis:\n  url: \
             \"redis://127.0.0.1:1\"\nsecurity:\n  signing_secret: \
             \"Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0=\"\n  credential_kek: \
             \"MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=\"\n",
        )
        .expect("a minimal config");
        let cmd = AdminCli::try_parse_from(["admin", "endpoint", "sync", "merge"])
            .expect("parses")
            .cmd;
        crate::admin::run(cmd, &db, &kek, "redis://127.0.0.1:1", &config)
            .await
            .expect_err("no database to read the endpoint from");
    }
}
