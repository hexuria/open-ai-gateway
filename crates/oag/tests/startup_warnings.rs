//! The warnings a running gateway actually emits.
//!
//! `settings::startup_warnings` is unit-tested where it lives, and that test
//! passes whether or not anything calls it — which is how R12's warning came to
//! be emitted from inside `settings::load`, before `init_telemetry` installs a
//! subscriber, and therefore to reach nobody at all. The unit test called the
//! function directly and was green throughout.
//!
//! So this runs the binary. `oag config` is the cheapest subcommand that takes
//! the same path a `serve` does — load, then telemetry, then the command — and
//! needs no database, no Redis and no network.

use std::io::Write as _;
use std::process::Command;

/// A config that validates, with the one setting R12 is about.
fn config_file(name: &str, usage_poll_interval: u64) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("oag-startup-{name}.yaml"));
    let mut f = std::fs::File::create(&path).expect("write a config");
    write!(
        f,
        r#"
database:
  url: "postgres://oag:oag@127.0.0.1:1/oag"
redis:
  url: "redis://127.0.0.1:1"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
gateway:
  usage_poll_interval: {usage_poll_interval}
"#
    )
    .expect("write a config");
    path
}

/// Everything the process wrote, on either stream.
///
/// `tracing_subscriber::fmt()` writes to **stdout**, not stderr — so a check
/// that read stderr alone would find nothing and could never fail, which is
/// the shape of test this round exists to remove. Both are read, and which one
/// carries the line is not what is being asserted.
fn output_of(path: &std::path::Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_oag"))
        .args(["--config", path.to_str().expect("utf-8"), "config"])
        // The default filter, whatever it is, has to let a warning through —
        // that is the point. Overriding it here would prove nothing.
        .env_remove("RUST_LOG")
        .output()
        .expect("the binary runs");
    assert!(out.status.success(), "`oag config` exited {}", out.status);
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// R12, from the outside: a zero poll interval is warned about on the way up.
///
/// Zero is accepted deliberately — a deployment with no seats has no reserve to
/// protect — but it also silently disables every seat's `usage_reserve_pct`,
/// which is a different feature from the one the operator turned off. Saying so
/// is the whole of the remedy, so it has to actually be said.
#[test]
fn a_zero_poll_interval_is_warned_about_where_an_operator_can_see_it() {
    let quiet = output_of(&config_file("sixty", 60));
    assert!(
        !quiet.contains("usage_reserve_pct"),
        "a positive interval has nothing to warn about: {quiet}"
    );

    let warned = output_of(&config_file("zero", 0));
    assert!(
        warned.contains("usage_reserve_pct"),
        "a zero interval has to reach the operator's terminal, not a `tracing` \
         with no subscriber installed — which is where it went for as long as \
         it was emitted from inside `settings::load`:\n{warned}"
    );
    assert!(
        warned.contains("WARN"),
        "and at a level that survives the default filter: {warned}"
    );
}
