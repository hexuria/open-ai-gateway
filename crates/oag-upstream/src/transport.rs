//! How bytes reach a provider.
//!
//! [`Transport`] exists as a trait for one reason: sub2api needs to impersonate
//! the TLS fingerprint of the official CLI, because it routes resold
//! subscription traffic that providers actively try to detect. We do not — an
//! internal gateway on sanctioned credentials has nothing to hide — so the only
//! implementation here is plain `reqwest` over rustls, and the build links no
//! BoringSSL.
//!
//! The seam stays because "we never need this" and "this is impossible to add"
//! are different claims, and only the first one is true.

use async_trait::async_trait;
use oag_core::{AccountId, Result};
use std::sync::Arc;
use std::time::Duration;

/// Identifies a connection pool.
///
/// Keyed by credential *and* egress proxy, not just host. Two credentials
/// sharing a TCP connection means they share whatever per-connection state the
/// provider keeps, and an upstream that decides to rate limit a connection
/// takes both down together.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransportKey {
    pub account: AccountId,
    pub proxy: Option<String>,
}

/// Sends a prepared request and yields a streaming response.
#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    async fn execute(&self, req: reqwest::Request) -> Result<reqwest::Response>;
}

/// `reqwest` over rustls.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
    /// How long to wait for the response headers. See `execute`.
    response_timeout: Duration,
}

impl HttpTransport {
    pub fn new(
        proxy: Option<&str>,
        connect_timeout: Duration,
        response_timeout: Duration,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            // No total-response timeout on the client. A streamed completion
            // legitimately runs for many minutes, and a deadline on the whole
            // response would sever it mid-answer. What IS bounded is the wait
            // for the response headers, in `execute` — the client's own
            // `.timeout()` cannot express "headers only", so it is done there.
            // Once a response exists, the server's idle watchdog takes over
            // and can tell "slow" from "dead"; before one does, nothing else
            // could.
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .http2_adaptive_window(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .user_agent(concat!("open-ai-gateway/", env!("CARGO_PKG_VERSION")));

        if let Some(url) = proxy {
            let p = reqwest::Proxy::all(url)
                .map_err(|e| oag_core::Error::Internal(format!("proxy {url}: {e}")))?;
            builder = builder.proxy(p);
        }

        let client = builder
            .build()
            .map_err(|e| oag_core::Error::Internal(format!("building http client: {e}")))?;
        Ok(Self {
            client,
            response_timeout,
        })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn execute(&self, req: reqwest::Request) -> Result<reqwest::Response> {
        // `client.execute` resolves when the response HEADERS arrive; the body
        // streams after. So a deadline here bounds exactly the silent gap — a
        // provider that accepted the connection and then sent nothing — and
        // ends the moment the answer begins, however long that answer then
        // takes. Before this, that gap had no bound at all: the request, its
        // credential slot and its in-flight guard sat until the provider felt
        // like it, and the breaker never heard, because nothing had failed.
        match tokio::time::timeout(self.response_timeout, self.client.execute(req)).await {
            Ok(result) => {
                result.map_err(|e| oag_core::Error::Internal(format!("upstream request: {e}")))
            }
            Err(_) => Err(oag_core::Error::UpstreamTimeout {
                after: self.response_timeout,
            }),
        }
    }
}

/// Bounded cache of transports, one per credential and proxy.
///
/// Bounded because a large credential pool would otherwise hold an unbounded
/// number of connection pools open. `moka` evicts by idle time, and an evicted
/// transport's in-flight requests are unaffected — the `Arc` outlives the cache
/// entry, so a long-running stream is never cut short by eviction. That is the
/// bug worth avoiding here.
#[derive(Debug, Clone)]
pub struct TransportPool {
    inner: moka::future::Cache<TransportKey, Arc<HttpTransport>>,
    connect_timeout: Duration,
    response_timeout: Duration,
}

impl TransportPool {
    #[must_use]
    pub fn new(
        max_entries: u64,
        idle_ttl: Duration,
        connect_timeout: Duration,
        response_timeout: Duration,
    ) -> Self {
        Self {
            inner: moka::future::Cache::builder()
                .max_capacity(max_entries)
                .time_to_idle(idle_ttl)
                .build(),
            connect_timeout,
            response_timeout,
        }
    }

    pub async fn get(&self, key: &TransportKey) -> Result<Arc<HttpTransport>> {
        if let Some(t) = self.inner.get(key).await {
            return Ok(t);
        }
        let transport = Arc::new(HttpTransport::new(
            key.proxy.as_deref(),
            self.connect_timeout,
            self.response_timeout,
        )?);
        self.inner.insert(key.clone(), Arc::clone(&transport)).await;
        Ok(transport)
    }

    #[must_use]
    pub fn len(&self) -> u64 {
        self.inner.entry_count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_provider_that_accepts_and_never_answers_is_a_bounded_error() {
        // The gap nothing bounded. A listener that completes the TCP accept
        // and then holds the socket open forever: the connect timeout is
        // satisfied, no response ever begins for the idle watchdog to
        // measure. Before, `execute` awaited this for as long as the peer
        // cared to keep the socket open.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hold = tokio::spawn(async move {
            let mut kept = Vec::new();
            loop {
                if let Ok((sock, _)) = listener.accept().await {
                    kept.push(sock);
                }
            }
        });

        let transport =
            HttpTransport::new(None, Duration::from_secs(5), Duration::from_millis(300))
                .expect("transport");
        let req = reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .body("{}")
            .build()
            .expect("request");

        let started = std::time::Instant::now();
        let err = transport.execute(req).await.expect_err("must not hang");
        hold.abort();

        assert!(
            matches!(err, oag_core::Error::UpstreamTimeout { after } if after == Duration::from_millis(300)),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "returned in {:?}, which is not bounded by the deadline",
            started.elapsed()
        );
        assert!(
            matches!(
                err.disposition(),
                oag_core::Disposition::FailoverAccount { .. }
            ),
            "a silent provider is failed over, like a 5xx"
        );
    }

    #[tokio::test]
    async fn transports_are_reused_per_credential() {
        let pool = TransportPool::new(
            16,
            Duration::from_mins(1),
            Duration::from_secs(5),
            Duration::from_secs(30),
        );
        let key = TransportKey {
            account: AccountId::new(),
            proxy: None,
        };
        let a = pool.get(&key).await.unwrap();
        let b = pool.get(&key).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same credential should share a pool");
    }

    #[tokio::test]
    async fn different_credentials_do_not_share_connections() {
        // Sharing would mean sharing whatever per-connection state the provider
        // keeps, so a rate limit on one takes the other down with it.
        let pool = TransportPool::new(
            16,
            Duration::from_mins(1),
            Duration::from_secs(5),
            Duration::from_secs(30),
        );
        let a = pool
            .get(&TransportKey {
                account: AccountId::new(),
                proxy: None,
            })
            .await
            .unwrap();
        let b = pool
            .get(&TransportKey {
                account: AccountId::new(),
                proxy: None,
            })
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn the_same_credential_through_different_proxies_is_separate() {
        let pool = TransportPool::new(
            16,
            Duration::from_mins(1),
            Duration::from_secs(5),
            Duration::from_secs(30),
        );
        let account = AccountId::new();
        let direct = pool
            .get(&TransportKey {
                account,
                proxy: None,
            })
            .await
            .unwrap();
        let viaproxy = pool
            .get(&TransportKey {
                account,
                proxy: Some("http://127.0.0.1:3128".to_owned()),
            })
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&direct, &viaproxy));
    }
}
