//! Stamps the build's commit into the binary, so a running gateway can say
//! what it is.
//!
//! Three incidents in one week came from somebody reasoning about a *running
//! process* from a *source tree*, because there was no third thing to ask. The
//! sharpest was a replica answering `{"ready":true,"schema":true}` while the
//! binary was days old — the health check standing as an alibi for exactly the
//! failure it looks like it covers. A process start time is not a build
//! identity, and inferring one from the other is what went wrong each time.
//!
//! `OAG_BUILD_SHA` wins if set, because the container build has no `.git`:
//! `deploy/Dockerfile` copies `crates`, `migrations` and `web`, and nothing
//! else. CI passes it as a build argument. Falling back to `git` covers a
//! local `cargo build`, and "unknown" is the honest answer when neither is
//! available — better than a plausible wrong value.

use std::process::Command;

fn main() {
    // Re-run when HEAD moves, or this is stamped once and stale forever.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-env-changed=OAG_BUILD_SHA");

    let sha = std::env::var("OAG_BUILD_SHA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=OAG_BUILD_SHA={sha}");
}
