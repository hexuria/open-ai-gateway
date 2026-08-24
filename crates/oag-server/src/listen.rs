//! The accept loop, with the connection deadlines actually applied.
//!
//! `axum::serve` is a convenience wrapper that builds a
//! `hyper_util::server::conn::auto::Builder` per connection and exposes none of
//! it. So `server.header_read_timeout` and `server.idle_timeout` were typed,
//! documented, environment-overridable — and read by nothing. A client that
//! opened a socket and dribbled one header byte per minute held a connection,
//! and a worker task, for as long as it liked.
//!
//! This is `axum::serve`'s own loop, kept deliberately close to it so the
//! shutdown behaviour is the same one the drain test exercises, with the
//! builder configured on the way past. `ConnectInfo` is not supported here
//! because nothing in this workspace extracts it.

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use oag_core::config::ServerConfig;
use std::pin::pin;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// The per-connection deadlines, as the HTTP layer understands them.
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    /// How long a client may take to send its request headers. HTTP/1 only —
    /// an HTTP/2 request head arrives in one frame sequence that hyper bounds
    /// separately.
    pub header_read_timeout: Duration,
    /// How long an HTTP/2 connection may go quiet before it is pinged.
    pub idle_timeout: Duration,
}

impl From<&ServerConfig> for Deadlines {
    fn from(cfg: &ServerConfig) -> Self {
        Self {
            header_read_timeout: cfg.header_read_timeout,
            idle_timeout: cfg.idle_timeout,
        }
    }
}

impl Deadlines {
    /// Zero means "no deadline", which is the only way to express that in a
    /// `Duration`-typed config field.
    fn optional(d: Duration) -> Option<Duration> {
        (!d.is_zero()).then_some(d)
    }

    fn builder(self) -> Builder<TokioExecutor> {
        let mut builder = Builder::new(TokioExecutor::new());
        builder
            .http1()
            // `header_read_timeout` is a no-op without a timer, silently.
            .timer(TokioTimer::new())
            .header_read_timeout(Self::optional(self.header_read_timeout));
        builder
            .http2()
            .timer(TokioTimer::new())
            .keep_alive_interval(Self::optional(self.idle_timeout))
            // Websockets over HTTP/2. Set because `axum::serve` sets it, so
            // that replacing it changes deadlines and nothing else.
            .enable_connect_protocol();
        builder
    }
}

/// Serve `router` on `listener` until `signal` resolves, then drain.
///
/// The sequence after the signal is `axum::serve`'s: stop accepting, tell every
/// live connection to shut down gracefully, and wait for them. `signal` itself
/// carries the drain budget — see [`crate::shutdown::signal`] — so by the time
/// it resolves the in-flight work has either finished or overrun.
pub async fn serve<F>(listener: TcpListener, router: Router, deadlines: Deadlines, signal: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    // Dropping the receiver is the signal; `closed()` on the sender is how each
    // connection task waits for it without a broadcast channel per connection.
    let (signal_tx, signal_rx) = watch::channel(());
    tokio::spawn(async move {
        signal.await;
        drop(signal_rx);
    });

    // The mirror image: the last connection task drops the last `close_rx`,
    // and `closed()` on the sender is how this function waits for all of them.
    let (close_tx, close_rx) = watch::channel(());

    loop {
        let io = tokio::select! {
            conn = accept(&listener) => conn,
            () = signal_tx.closed() => break,
        };

        let service = TowerToHyperService::new(router.clone());
        let signal_tx = signal_tx.clone();
        let close_rx = close_rx.clone();

        tokio::spawn(async move {
            let builder = deadlines.builder();
            let mut conn = pin!(builder.serve_connection_with_upgrades(TokioIo::new(io), service));

            let mut told_to_stop = false;
            loop {
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(e) = result {
                            tracing::trace!(error = %e, "connection closed");
                        }
                        break;
                    }
                    () = signal_tx.closed(), if !told_to_stop => {
                        told_to_stop = true;
                        conn.as_mut().graceful_shutdown();
                    }
                }
            }

            drop(close_rx);
        });
    }

    drop(close_rx);
    drop(listener);
    close_tx.closed().await;
}

/// Accept, treating a client that hung up between SYN and accept as normal and
/// an exhausted file-descriptor table as something to back off from rather than
/// spin on.
async fn accept(listener: &TcpListener) -> tokio::net::TcpStream {
    loop {
        match listener.accept().await {
            Ok((io, _)) => return io,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::ConnectionReset
                ) => {}
            Err(e) => {
                // Typically EMFILE. Retrying immediately turns it into a busy
                // loop that keeps the process from closing anything.
                tracing::error!(error = %e, "accept failed; retrying in a second");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A listener on an ephemeral port, the router that answers on it, and the
    /// handle that stops it.
    async fn running(
        deadlines: Deadlines,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let router = Router::new().route("/ok", get(|| async { "ok" }));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(serve(listener, router, deadlines, async move {
            let _ = stopped.await;
        }));
        (addr, stop, task)
    }

    #[tokio::test]
    async fn a_request_is_served_and_the_signal_stops_the_listener() {
        // The loop below is hand-rolled because `axum::serve` exposes no
        // connection configuration. That makes serving a request, and stopping
        // when told to, things this module has to prove rather than inherit.
        let (addr, stop, task) = running(Deadlines {
            header_read_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_mins(2),
        })
        .await;

        let body = reqwest::get(format!("http://{addr}/ok"))
            .await
            .expect("request")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "ok");

        let _ = stop.send(());
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the accept loop must return once the signal fires")
            .expect("task");
    }

    #[tokio::test]
    async fn a_client_that_never_finishes_its_headers_is_hung_up_on() {
        // The finding. `server.header_read_timeout` was config, documentation
        // and an environment override with nothing reading it, so a client
        // could open a socket, send half a request line, and hold a connection
        // for as long as it liked — the whole of a slowloris, and free.
        let (addr, _stop, _task) = running(Deadlines {
            header_read_timeout: Duration::from_millis(250),
            idle_timeout: Duration::from_mins(2),
        })
        .await;

        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        // A request that is never terminated by the blank line.
        sock.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .expect("write");

        // hyper answers 408 and closes, so the read ends rather than blocking.
        let mut sink = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut sink))
            .await
            .expect("the connection must not outlive the header deadline")
            .expect("read");
    }

    #[test]
    fn a_zero_duration_disables_the_deadline_rather_than_expiring_instantly() {
        // The config type cannot express `None`, so zero has to mean it. A
        // literal zero header-read timeout would refuse every connection.
        assert_eq!(Deadlines::optional(Duration::ZERO), None);
        assert_eq!(
            Deadlines::optional(Duration::from_secs(10)),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn the_configured_deadlines_are_the_ones_carried() {
        let cfg = ServerConfig {
            header_read_timeout: Duration::from_secs(7),
            idle_timeout: Duration::from_secs(31),
            ..ServerConfig::default()
        };
        let deadlines = Deadlines::from(&cfg);
        assert_eq!(deadlines.header_read_timeout, Duration::from_secs(7));
        assert_eq!(deadlines.idle_timeout, Duration::from_secs(31));
    }
}
