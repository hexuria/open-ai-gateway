#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Talking to providers.

pub mod adapter;
pub mod anthropic;
pub mod bedrock;
pub mod codex;
pub mod eventstream;
pub mod gemini;
pub mod openai;
pub mod openai_oauth;
pub mod pricing;
pub mod sigv4;
pub mod transport;
pub mod usage;
pub mod xai_oauth;

pub use adapter::{Framing, ProviderAdapter, UpstreamRequest};
pub use anthropic::AnthropicAdapter;
pub use bedrock::BedrockAdapter;
pub use codex::CodexAdapter;
pub use gemini::GeminiAdapter;
pub use openai::OpenAICompatAdapter;
pub use transport::{HttpTransport, Transport, TransportKey, TransportPool};

/// The `reqwest::Client` every adapter builds its requests through.
///
/// Adapters do not *execute* requests — `transport` does, with its own
/// per-account client and proxy — so this exists only to be a request builder.
/// It was `reqwest::Client::new()` at each call site, once per request, which
/// is two problems in one line.
///
/// `Client::new()` panics if the TLS backend cannot initialise, and this crate
/// denies `unwrap` everywhere else for exactly that reason: a panic on the
/// request path severs the connection with no response, and on HTTP/2 resets
/// every other stream multiplexed onto it. Built once here, a failure is
/// something the process can notice at first use rather than mid-request.
///
/// And each `Client` allocates a connection pool that this use throws away
/// immediately — invisible per request, and one allocation and teardown per
/// request across every adapter.
pub(crate) fn builder_client() -> oag_core::Result<reqwest::Client> {
    // Built once, and the outcome — including a failure — is what is cached.
    //
    // This used to end in `unwrap_or_default()`, under a comment saying that
    // was "rather than a panic". It is not: `Client::default()` is
    // `Client::new()`, which reqwest documents as panicking, so the fallback
    // was the same panic one call deeper. The comment also promised the failure
    // would surface "at the transport with an error naming it"; the transport
    // is never reached, because the panic happens while building the request.
    //
    // A `Result` instead. Every caller is inside an adapter's `build`, which
    // already returns one, so this costs five `?` and turns a panic on the
    // request path — a 500 for every in-flight stream on that replica, which is
    // exactly what this crate's lint configuration forbids — into an error the
    // caller reports.
    //
    // The error is cached with the client because a TLS backend that failed to
    // initialise will fail identically every time, and retrying it per request
    // would turn one broken deployment into a busy one.
    static CLIENT: std::sync::OnceLock<std::result::Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .build()
                .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| {
            oag_core::Error::Internal(format!(
                "the HTTP client could not be built, so no upstream can be reached \
                 from this process: {e}"
            ))
        })
}

/// A client for the calls that are not inference: refresh, quota, prices.
///
/// Proxy-aware, which is the whole reason it exists. `proxy_url` is set per
/// credential and was applied only by `transport`, so a deployment whose egress
/// must go through a proxy had its refresh, quota and price traffic leave by a
/// different route — sometimes failing, sometimes succeeding and quietly
/// bypassing the control the proxy was there to enforce. A credential's proxy
/// is a property of the credential, not of one kind of request made with it.
///
/// Built per call rather than cached: these run on a poller or a refresh, not
/// per request, and caching per proxy string would be a map to invalidate for
/// no measurable gain.
///
/// # Errors
///
/// If the proxy URL is unusable or the client cannot be built.
pub fn side_channel_client(
    proxy: Option<&str>,
    timeout: std::time::Duration,
) -> oag_core::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if let Some(url) = proxy.map(str::trim).filter(|u| !u.is_empty()) {
        let proxy = reqwest::Proxy::all(url).map_err(|e| {
            oag_core::Error::Config(format!("credential proxy_url {url} is unusable: {e}"))
        })?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| oag_core::Error::Internal(format!("building a side-channel client: {e}")))
}

#[cfg(test)]
mod side_channel_tests {
    use super::side_channel_client;
    use std::time::Duration;

    /// U12. A credential's proxy applies to every call made with it.
    ///
    /// `proxy_url` is set per credential and was applied only by `transport`,
    /// so a deployment whose egress must go through a proxy had its refresh,
    /// quota and price traffic leave by a different route — sometimes failing,
    /// sometimes succeeding and quietly bypassing the control the proxy existed
    /// to enforce. A credential's proxy is a property of the credential, not of
    /// one kind of request made with it.
    /// C5: `builder_client` reports a failure instead of unwinding.
    ///
    /// It ended in `unwrap_or_default()` under a comment calling that an
    /// alternative to panicking. `Client::default()` is `Client::new()`, which
    /// reqwest documents as panicking — so the fallback was the same panic one
    /// call deeper, on a path this crate's lints forbid panicking on because a
    /// panic there is a 500 for every in-flight stream on the replica. The
    /// comment also promised the failure would surface "at the transport with
    /// an error naming it"; the transport is never reached.
    ///
    /// This asserts what a test here can: the call yields `Ok` and is reusable.
    /// The failure itself cannot be provoked in a unit test — it needs a broken
    /// TLS backend, not an input — and that is precisely why the old claim went
    /// unexamined. What stops it now is the signature: `?` at all five call
    /// sites, each already inside a `build` that returns `Result`, so there is
    /// no longer an expression that *can* unwind.
    #[test]
    fn the_shared_client_is_built_fallibly() {
        super::builder_client().expect("a client is built");
        super::builder_client().expect("and the cached outcome is reusable");
    }

    #[test]
    fn a_side_channel_client_accepts_a_proxy_and_refuses_a_broken_one() {
        side_channel_client(None, Duration::from_secs(20)).expect("no proxy is fine");
        side_channel_client(Some("http://127.0.0.1:3128"), Duration::from_secs(20))
            .expect("a usable proxy is configured, not ignored");

        // Blank is treated as absent rather than as a proxy called "": an empty
        // column and a NULL one mean the same thing to an operator.
        side_channel_client(Some("   "), Duration::from_secs(20)).expect("blank is absent");

        // And an unusable one is a config error, said here, rather than silent
        // direct egress — which is the failure mode the whole finding is about.
        let err =
            side_channel_client(Some("not a url"), Duration::from_secs(20)).expect_err("refused");
        assert!(err.to_string().contains("proxy_url"), "{err}");
    }
}
