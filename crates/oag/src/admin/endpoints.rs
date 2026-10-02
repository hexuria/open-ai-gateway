//! `oag admin endpoint`: the upstreams an operator registers.
//!
//! Every write goes through `oag_server::endpoints`, the module the admin API
//! writes through too, so the CLI and the console refuse the same rows for the
//! same reasons. This process serves nothing: a running gateway picks a write
//! up on its next catalog refresh.

use super::accounts::price_account;
use super::{EndpointAddArgs, EndpointCommand, EndpointSetArgs};
use oag_core::config::Config;
use oag_core::provider::Platform;
use oag_core::{Kek, Provider, Result, credential::SecretMaterial};
use oag_server::endpoints::{self, Checked, Draft};
use oag_store::repo::{self, EndpointDeletion, EndpointReferences};
use oag_store::{Db, EndpointRow};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// What every write says about when it takes effect.
const REFRESH: &str =
    "  a running gateway serves the change from its next catalog refresh; no restart";

/// Every `oag admin endpoint` verb. `sync` and `models` read the endpoint's
/// own model list, and are `endpoint_sync`'s.
pub(super) async fn endpoint_cmd(
    db: &Db,
    kek: &Kek,
    config: &Config,
    cmd: EndpointCommand,
) -> Result<()> {
    match cmd {
        EndpointCommand::Add { args } => add(db, args).await,
        EndpointCommand::List => list(db).await,
        EndpointCommand::Show { name } => show(db, &name).await,
        EndpointCommand::Set { args } => set(db, args).await,
        EndpointCommand::Remove { name } => remove(db, &name).await,
        EndpointCommand::Check { name, account } => check(db, kek, &name, account.as_deref()).await,
        EndpointCommand::Catalog(cmd) => super::endpoint_sync::run(db, kek, config, cmd).await,
    }
}

fn not_found(name: &str) -> oag_core::Error {
    oag_core::Error::Config(format!(
        "no endpoint named '{name}'; see `oag admin endpoint list`"
    ))
}

/// `add`'s flags as the row to register.
pub(super) fn draft(args: EndpointAddArgs) -> Result<Draft> {
    let platform = args.platform.platform();
    let auth = args
        .auth
        .map_or_else(|| endpoints::default_auth(platform), super::AuthArg::style);
    Ok(Draft {
        name: args.name,
        dialect: args.dialect.column().to_owned(),
        platform: platform.as_str().to_owned(),
        base_url: args.base_url,
        auth: auth.as_str().to_owned(),
        region: args.region,
        project: args.project,
        api_version: args.api_version,
        path: args.path,
        extra_headers: Value::Object(parse_headers(&args.headers)?.into_iter().collect()),
        display_name: args.display_name,
        discover_models: args.discover,
    })
}

/// `NAME=VALUE` pairs as header names and values, in the order given.
///
/// The name is everything before the first `=`, so a value may hold one. A
/// pair is never quoted back: whatever was typed after the `=` is a value the
/// operator may regret typing, and this would copy it into a terminal log.
pub(super) fn parse_headers(pairs: &[String]) -> Result<Vec<(String, Value)>> {
    let mut headers: Vec<(String, Value)> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let Some((name, value)) = pair.split_once('=') else {
            return Err(oag_core::Error::Config(
                "a --header has no `=`: give it as NAME=VALUE".to_owned(),
            ));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(oag_core::Error::Config(
                "a --header has no name before its `=`".to_owned(),
            ));
        }
        if headers
            .iter()
            .any(|(given, _)| given.eq_ignore_ascii_case(name))
        {
            return Err(oag_core::Error::Config(format!(
                "header `{name}` is given twice"
            )));
        }
        headers.push((name.to_owned(), Value::String(value.trim().to_owned())));
    }
    Ok(headers)
}

/// `set`'s flags applied to the stored row. Only a flag that was given changes
/// anything, and an empty value clears a setting that may be empty.
pub(super) fn apply(draft: &mut Draft, args: EndpointSetArgs) -> Result<()> {
    let EndpointSetArgs {
        name: _,
        base_url,
        auth,
        headers,
        unset_headers,
        region,
        project,
        api_version,
        path,
        display_name,
        discover,
    } = args;
    let given = base_url.is_some()
        || auth.is_some()
        || !headers.is_empty()
        || !unset_headers.is_empty()
        || region.is_some()
        || project.is_some()
        || api_version.is_some()
        || path.is_some()
        || display_name.is_some()
        || discover.is_some();
    if !given {
        return Err(oag_core::Error::Config(
            "nothing to change: name a setting, such as --base-url, --auth, --header, \
             --unset-header, --region, --project, --api-version, --path, --display-name or \
             --discover"
                .to_owned(),
        ));
    }
    // Blank values are cleared by the shared rules, which read blank as none.
    for (field, value) in [
        (&mut draft.base_url, base_url),
        (&mut draft.region, region),
        (&mut draft.project, project),
        (&mut draft.api_version, api_version),
        (&mut draft.path, path),
        (&mut draft.display_name, display_name),
    ] {
        if let Some(value) = value {
            *field = Some(value);
        }
    }
    if let Some(auth) = auth {
        auth.style().as_str().clone_into(&mut draft.auth);
    }
    if let Some(discover) = discover {
        draft.discover_models = discover;
    }
    let mut kept: Map<String, Value> = draft.extra_headers.as_object().cloned().unwrap_or_default();
    for name in &unset_headers {
        let before = kept.len();
        kept.retain(|stored, _| !stored.eq_ignore_ascii_case(name));
        if kept.len() == before {
            return Err(oag_core::Error::Config(format!(
                "endpoint '{}' sends no header `{name}` to unset",
                draft.name
            )));
        }
    }
    for (name, value) in parse_headers(&headers)? {
        kept.retain(|stored, _| !stored.eq_ignore_ascii_case(&name));
        kept.insert(name, value);
    }
    draft.extra_headers = Value::Object(kept);
    Ok(())
}

async fn add(db: &Db, args: EndpointAddArgs) -> Result<()> {
    let draft = draft(args)?;
    let name = draft.name.clone();
    let row = endpoints::register(db, draft)
        .await
        .map_err(|e| e.into_error(&name))?;
    for line in added_lines(&row) {
        println!("{line}");
    }
    Ok(())
}

/// How `account add` is handed a key for an endpoint on `platform`: a gcp
/// endpoint's credential is a service account's JSON key, read whole from the
/// file Google issues; every other platform's is one line.
pub(super) fn secret_flag(platform: &str) -> &'static str {
    if platform == Platform::Gcp.as_str() {
        "--secret-file <service-account.json>"
    } else {
        "--secret <key>"
    }
}

/// Where `row`'s endpoint answers: its base URL, or what its platform builds
/// one from.
fn place(row: &EndpointRow) -> String {
    match (&row.base_url, &row.region, &row.project) {
        (Some(url), _, _) => url.clone(),
        (None, Some(region), Some(project)) => format!("project {project} in {region}"),
        (None, Some(region), None) => format!("region {region}"),
        (None, None, _) => "-".to_owned(),
    }
}

/// What `add` prints: the endpoint, what it will not do yet, and the two
/// commands that make it serve.
pub(super) fn added_lines(row: &EndpointRow) -> Vec<String> {
    let name = &row.name;
    let mut lines = vec![format!(
        "endpoint {name}: {} on {} at {}",
        row.dialect,
        row.platform,
        place(row)
    )];
    let headers: Vec<String> = endpoints::shown_headers(&row.extra_headers)
        .into_iter()
        .map(|(header, _)| header)
        .collect();
    lines.push(if headers.is_empty() {
        format!("  auth {}; no extra headers", row.auth)
    } else {
        format!("  auth {}; headers {}", row.auth, headers.join(", "))
    });
    if let Some(refusal) = endpoints::refusal(row) {
        lines.push(format!(
            "  not served by this build: {refusal}. The row is kept for a build that serves it"
        ));
    }
    lines.push(format!(
        "  next: oag admin account add --name {name}-1 --provider {name} {}",
        secret_flag(&row.platform)
    ));
    lines.push(format!(
        "        oag admin catalog add --id {name}/<model> --upstream <model> \
         --input-per-mtok <usd> --output-per-mtok <usd> --context <tokens> --max-output <tokens>"
    ));
    lines.push(REFRESH.to_owned());
    lines
}

async fn list(db: &Db) -> Result<()> {
    let rows = repo::list_endpoints(db).await?;
    let refs = repo::endpoint_references(db).await?;
    for line in list_lines(&rows, &refs) {
        println!("{line}");
    }
    Ok(())
}

/// What `list` prints. Headers and a reason the gateway skips a row go on
/// lines of their own under it, so the table stays a table.
pub(super) fn list_lines(
    rows: &[EndpointRow],
    refs: &HashMap<String, EndpointReferences>,
) -> Vec<String> {
    if rows.is_empty() {
        return vec!["no endpoints; register one with `oag admin endpoint add`".to_owned()];
    }
    let mut lines = vec![format!(
        "{:<20} {:<10} {:<8} {:<14} {:>8} {:>6} {:>9}  WHERE",
        "NAME", "DIALECT", "PLATFORM", "AUTH", "ACCOUNTS", "MODELS", "ON LADDER"
    )];
    for row in rows {
        let counts = refs.get(&row.name).copied().unwrap_or_default();
        lines.push(format!(
            "{:<20} {:<10} {:<8} {:<14} {:>8} {:>6} {:>9}  {}",
            row.name,
            row.dialect,
            row.platform,
            row.auth,
            counts.accounts,
            counts.models,
            counts.on_ladder,
            place(row)
        ));
        for (header, value) in endpoints::shown_headers(&row.extra_headers) {
            lines.push(format!("{:21}header {header}: {value}", ""));
        }
        if let Some(refusal) = endpoints::refusal(row) {
            lines.push(format!("{:21}not served: {refusal}", ""));
        }
    }
    lines
}

async fn show(db: &Db, name: &str) -> Result<()> {
    let Some(row) = repo::get_endpoint(db, name).await? else {
        return Err(not_found(name));
    };
    let refs = repo::endpoint_references(db).await?;
    for line in show_lines(&row, refs.get(name).copied().unwrap_or_default()) {
        println!("{line}");
    }
    Ok(())
}

/// What `show` prints: every setting, what names the endpoint, and whether
/// this build serves it.
pub(super) fn show_lines(row: &EndpointRow, refs: EndpointReferences) -> Vec<String> {
    let or_dash = |value: Option<&str>| value.unwrap_or("-").to_owned();
    let mut lines = vec![
        format!("name          {}", row.name),
        format!("display name  {}", or_dash(row.display_name.as_deref())),
        format!("dialect       {}", row.dialect),
        format!("platform      {}", row.platform),
        format!("base url      {}", or_dash(row.base_url.as_deref())),
        format!("auth          {}", row.auth),
        format!("region        {}", or_dash(row.region.as_deref())),
        format!("project       {}", or_dash(row.project.as_deref())),
        format!("api version   {}", or_dash(row.api_version.as_deref())),
        format!("path          {}", or_dash(row.path.as_deref())),
        format!(
            "discover      {}",
            if row.discover_models { "yes" } else { "no" }
        ),
    ];
    let headers = endpoints::shown_headers(&row.extra_headers);
    if headers.is_empty() {
        lines.push("headers       -".to_owned());
    }
    for (i, (header, value)) in headers.iter().enumerate() {
        let label = if i == 0 { "headers" } else { "" };
        lines.push(format!("{label:<14}{header}: {value}"));
    }
    lines.push(format!(
        "accounts      {} ({} in rotation)",
        refs.accounts, refs.schedulable
    ));
    lines.push(format!("models        {}", refs.models));
    lines.push(format!("on a ladder   {}", refs.on_ladder));
    lines.push(match endpoints::refusal(row) {
        None => "served        yes".to_owned(),
        Some(refusal) => format!("served        no: {refusal}"),
    });
    lines
}

async fn set(db: &Db, args: EndpointSetArgs) -> Result<()> {
    let name = args.name.clone();
    let Some(stored) = repo::get_endpoint(db, &name).await? else {
        return Err(not_found(&name));
    };
    let mut draft = Draft::from_row(&stored);
    apply(&mut draft, args)?;
    let row = endpoints::change(db, &stored, draft)
        .await
        .map_err(|e| e.into_error(&name))?;
    println!(
        "endpoint {name} updated: {} on {} at {}",
        row.dialect,
        row.platform,
        place(&row)
    );
    if let Some(refusal) = endpoints::refusal(&row) {
        println!("  not served by this build: {refusal}");
    }
    println!("{REFRESH}");
    Ok(())
}

async fn remove(db: &Db, name: &str) -> Result<()> {
    match repo::delete_endpoint(db, name).await? {
        EndpointDeletion::Deleted => {
            println!("removed endpoint {name}");
            println!("{REFRESH}");
            Ok(())
        }
        EndpointDeletion::NotFound => Err(not_found(name)),
        EndpointDeletion::InUse { accounts, models } => {
            for line in in_use_lines(name, accounts, models) {
                println!("{line}");
            }
            Err(oag_core::Error::Config(format!(
                "endpoint '{name}' is in use, so it was not removed"
            )))
        }
    }
}

/// What `remove` prints when something still names the endpoint: what does,
/// and how to clear each. Neither has a command of its own yet, so the way is
/// a statement against the database, and only after the requests they are
/// serving have finished.
pub(super) fn in_use_lines(name: &str, accounts: i64, models: i64) -> Vec<String> {
    let mut lines = vec![
        format!(
            "endpoint {name} is still named by {accounts} credential(s) and {models} catalog \
             model(s), so nothing was removed"
        ),
        "  removed first, it would leave them naming an upstream nothing serves".to_owned(),
    ];
    if accounts > 0 {
        lines.push(format!(
            "  credentials: take them out of rotation (`oag admin account list` shows them under \
             {name}; `oag admin account disable <credential>`), let their requests finish, then"
        ));
        lines.push(format!(
            "    psql \"$OAG_DATABASE__URL\" -c \"DELETE FROM account WHERE provider = '{name}'\""
        ));
    }
    if models > 0 {
        lines.push(
            "  models: take them off every ladder (`oag admin route show`, `oag admin route \
             tiers`), then"
                .to_owned(),
        );
        lines.push(format!(
            "    psql \"$OAG_DATABASE__URL\" -c \"DELETE FROM model_catalog WHERE provider = \
             '{name}'\""
        ));
    }
    lines.push(format!("  then: oag admin endpoint remove {name}"));
    lines
}

async fn check(db: &Db, kek: &Kek, name: &str, account: Option<&str>) -> Result<()> {
    let Some(row) = repo::get_endpoint(db, name).await? else {
        return Err(not_found(name));
    };
    let checked = match account {
        None => endpoints::check(&row, None, None).await,
        Some(account) => {
            let config = row.to_endpoint().map_err(|refusal| {
                oag_core::Error::Config(format!(
                    "endpoint '{name}' is not served, so none of its keys is sent anywhere: \
                     {refusal}"
                ))
            })?;
            let credential =
                price_account(db, Provider::Custom(config.endpoint), Some(account)).await?;
            let material: SecretMaterial = kek
                .open_json::<SecretMaterial>(&credential.sealed())?
                .trimmed();
            endpoints::check(
                &row,
                Some(&material.access_token),
                credential.proxy_url.as_deref(),
            )
            .await
        }
    };
    for line in check_lines(&checked, account) {
        println!("{line}");
    }
    if checked.ok() {
        Ok(())
    } else {
        Err(oag_core::Error::Config(format!(
            "endpoint '{name}' did not answer with a model list"
        )))
    }
}

/// What `check` prints: what was asked and with which key, then what came
/// back. Never the key.
pub(super) fn check_lines(checked: &Checked, account: Option<&str>) -> Vec<String> {
    let key = account.map_or_else(|| "no key".to_owned(), |name| format!("the key of {name}"));
    let mut lines = vec![match &checked.url {
        Some(url) => format!("GET {url} ({key})"),
        None => "nothing was asked".to_owned(),
    }];
    match (&checked.error, checked.models) {
        (None, Some(models)) => lines.push(format!(
            "  answered {}: {models} model(s) listed{}",
            checked.status.unwrap_or_default(),
            if checked.more {
                ", and more on pages not read"
            } else {
                ""
            }
        )),
        (error, _) => {
            lines.push(format!(
                "  failed: {}",
                error.as_deref().unwrap_or("no model list")
            ));
            if account.is_none() && matches!(checked.status, Some(401 | 403)) {
                lines.push(
                    "  it wants a key: check with one of its credentials, --account <name>"
                        .to_owned(),
                );
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests;
