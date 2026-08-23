//! Typed configuration.
//!
//! Configuration is a file plus environment overrides. All of it. Anything that
//! must change without a restart gets a **typed column on the entity it belongs
//! to** — `route.default_mode`, `account.schedulable` — never a generic
//! key-value row.
//!
//! There is no `setting` table. A tunable needs a reader and a write path
//! either way, so a generic row is not cheaper than a typed column; it is only
//! less checkable. sub2api put nearly everything in one and grew a 2466-line
//! handler parsing it. `state.reload_catalog()` shows restart-free mutation
//! without any of that.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub security: SecurityConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Inference traffic. This is the only listener the load balancer fronts.
    pub public_addr: String,
    /// Admin API, the SPA, `/metrics`, `/health/ready`. Bind this to the
    /// internal network. sub2api served admin and inference on one port, which
    /// means every admin endpoint inherits the public listener's exposure.
    pub admin_addr: String,
    /// How long a client may take to send its request headers.
    ///
    /// Applied to the HTTP/1 connection in `oag_server::serve`. Without it a
    /// trickle of header bytes holds a connection open indefinitely, which is
    /// the whole of a slowloris.
    #[serde(with = "humantime_secs")]
    pub header_read_timeout: Duration,
    /// How long an HTTP/2 connection may go quiet before it is pinged, and
    /// dropped if the ping is not answered.
    ///
    /// Deliberately *not* a whole-response write timeout: a streamed completion
    /// legitimately runs for many minutes and any total-response deadline will
    /// sever it. A ping is answered by the peer's transport, not by its
    /// application, so an idle-but-alive stream survives one.
    #[serde(with = "humantime_secs")]
    pub idle_timeout: Duration,
    /// Maximum inbound body.
    ///
    /// This is buffered in memory, per in-flight request, before the request is
    /// parsed. It used to default to 256 MiB — chosen for image and document
    /// payloads, and roughly a quarter of the memory limit the Helm chart asks
    /// for, so a handful of concurrent large POSTs was an OOM. 32 MiB still
    /// carries a base64 image well past any provider's own inline-attachment
    /// ceiling; a deployment that genuinely needs more can raise it, knowing
    /// what it is multiplying by its concurrency.
    pub max_body_bytes: usize,
    /// Serve admin routes on the public listener instead of their own.
    ///
    /// Off by default, because two listeners is the safer shape: it makes "do
    /// not expose the admin API" a deployment fact rather than a routing rule
    /// someone has to remember.
    ///
    /// Some platforms route to exactly **one** container port — Cloud Run and
    /// Azure Container Apps both do — and on those the admin listener is simply
    /// unreachable, which takes `/health/ready` and `/metrics` with it. This
    /// exists for them.
    ///
    /// What it costs is not symmetric. `/admin/api` keeps its admin-role key, so
    /// the writes lose the second layer and keep the first. The dashboard,
    /// `/metrics` and `/health/ready` sit outside that layer by design and never
    /// had a first layer to keep, so on a shared port they are simply
    /// unauthenticated. Restrict the service with the platform's ingress rules
    /// or IAM; the key is not doing that work for you.
    #[serde(default)]
    pub single_listener: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            public_addr: "0.0.0.0:8080".to_owned(),
            admin_addr: "127.0.0.1:8081".to_owned(),
            header_read_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_mins(2),
            max_body_bytes: 32 * 1024 * 1024,
            single_listener: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: String,
    #[serde(default = "default_db_pool")]
    pub max_connections: u32,
}

const fn default_db_pool() -> u32 {
    16
}

fn default_bedrock_region() -> String {
    "us-east-1".to_owned()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedisConfig {
    pub url: String,
}

/// Secrets. Every field here is required.
///
/// sub2api generates its signing secret at first boot and writes it to a local
/// file when the environment does not supply one. With more than one replica
/// and unshared volumes, replica A mints tokens replica B rejects — an
/// intermittent auth failure that looks like anything but a config problem.
/// We refuse to start instead. See [`SecurityConfig::validate`].
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    /// Signs admin session tokens. Must be identical across replicas.
    pub signing_secret: String,
    /// Key-encryption key for sealing upstream credentials at rest,
    /// base64-encoded, 32 bytes. Must be identical across replicas.
    pub credential_kek: String,
}

impl std::fmt::Debug for SecurityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityConfig")
            .field("signing_secret", &"<redacted>")
            .field("credential_kek", &"<redacted>")
            .finish()
    }
}

impl SecurityConfig {
    /// Reject secrets that would be unsafe or that indicate an unset value.
    ///
    /// Fail fast and loudly: a gateway that boots with a placeholder secret is
    /// worse than one that does not boot, because it looks like it is working.
    pub fn validate(&self) -> crate::Result<()> {
        if self.signing_secret.len() < 32 {
            return Err(crate::Error::Config(
                "security.signing_secret must be at least 32 bytes; \
                 generate one with `openssl rand -base64 48`"
                    .to_owned(),
            ));
        }
        if self.signing_secret.contains("change") || self.signing_secret.contains("example") {
            return Err(crate::Error::Config(
                "security.signing_secret still looks like a placeholder".to_owned(),
            ));
        }
        if self.credential_kek.is_empty() {
            return Err(crate::Error::Config(
                "security.credential_kek is required; \
                 generate one with `openssl rand -base64 32`"
                    .to_owned(),
            ));
        }
        // Parsed here, not just checked for emptiness, so every subcommand fails
        // the same way at config load. Previously only the paths that build a
        // `Kek` — `serve` and `admin` — noticed a malformed one, so `migrate`
        // succeeded and the gateway then crash-looped on boot with the real
        // reason buried in a restarting container's logs. Found exactly that way.
        crate::Kek::from_base64(&self.credential_kek)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GatewayConfig {
    /// How long an upstream stream may go without sending anything before we
    /// treat it as stalled and fail over.
    #[serde(with = "humantime_secs")]
    pub stream_idle_timeout: Duration,
    /// How often to emit a no-op event downstream, so intermediaries with their
    /// own idle timeouts do not drop a legitimately quiet stream.
    #[serde(with = "humantime_secs")]
    pub stream_keepalive_interval: Duration,
    /// Ceiling on a single stream. Also the shutdown drain budget: on SIGTERM
    /// we stop accepting work and give in-flight streams this long to finish.
    #[serde(with = "humantime_secs")]
    pub max_stream_duration: Duration,
    /// Attempts against one credential before failing over to another.
    pub same_account_retries: u8,
    /// Credentials to try before giving up on the request.
    pub max_account_switches: u8,
    /// How often to reload the model catalog from the database.
    ///
    /// The catalog is held in memory and swapped wholesale. Without a refresh,
    /// seeding or repricing a model needs every replica restarted before it
    /// takes effect — and the replicas give no sign that they are stale.
    #[serde(with = "humantime_secs")]
    pub catalog_refresh_interval: Duration,
    /// AWS region for Bedrock. Also part of the SigV4 signing scope, so it has
    /// to be right even when `provider_base_urls` points somewhere else.
    #[serde(default = "default_bedrock_region")]
    pub bedrock_region: String,
    /// Override a provider's base URL, keyed by provider name.
    ///
    /// For self-hosted or proxied endpoints, a regional deployment, or a mock
    /// during testing. Omitted providers use their public API.
    #[serde(default)]
    pub provider_base_urls: std::collections::BTreeMap<String, String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            stream_idle_timeout: Duration::from_mins(3),
            stream_keepalive_interval: Duration::from_secs(10),
            max_stream_duration: Duration::from_mins(30),
            same_account_retries: 2,
            max_account_switches: 3,
            catalog_refresh_interval: Duration::from_mins(1),
            bedrock_region: default_bedrock_region(),
            provider_base_urls: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct TelemetryConfig {
    /// `tracing` filter, e.g. `info,oag_server=debug`.
    pub log_filter: String,
    /// JSON lines rather than human-readable. On in production.
    pub log_json: bool,
    /// OTLP endpoint for distributed tracing.
    ///
    /// **Not implemented in this build.** No OTLP exporter is linked, so
    /// setting this is rejected at startup rather than silently ignored — a
    /// config option that quietly does nothing is worse than one that does not
    /// exist, because you only find out when you go looking for the traces.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_filter: "info".to_owned(),
            log_json: false,
            otlp_endpoint: None,
        }
    }
}

impl Config {
    /// Parse YAML and validate.
    pub fn from_yaml(src: &str) -> crate::Result<Self> {
        let cfg: Self = serde_yaml_ng::from_str(src)
            .map_err(|e| crate::Error::Config(format!("parsing config: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> crate::Result<()> {
        self.security.validate()?;
        if self.telemetry.otlp_endpoint.is_some() {
            return Err(crate::Error::Config(
                "telemetry.otlp_endpoint is set, but this build has no OTLP exporter linked.                  Unset it; metrics are on /metrics and logs are structured."
                    .to_owned(),
            ));
        }
        if self.gateway.max_stream_duration <= self.gateway.stream_idle_timeout {
            return Err(crate::Error::Config(
                "gateway.max_stream_duration must exceed gateway.stream_idle_timeout".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Serialise `Duration` as whole seconds. Keeps the config file readable
/// without pulling in a date-parsing dependency for four fields.
mod humantime_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
database:
  url: "postgres://oag:oag@localhost/oag"
redis:
  url: "redis://localhost:6379"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
"#;

    #[test]
    fn minimal_config_parses_with_defaults() {
        let cfg = Config::from_yaml(MINIMAL).expect("minimal config should parse");
        assert_eq!(cfg.server.public_addr, "0.0.0.0:8080");
        // The admin listener must not default to a public bind.
        assert!(cfg.server.admin_addr.starts_with("127.0.0.1"));
    }

    #[test]
    fn short_signing_secret_is_rejected() {
        let src = MINIMAL.replace(
            "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0=",
            "tooshort",
        );
        assert!(Config::from_yaml(&src).is_err());
    }

    #[test]
    fn placeholder_signing_secret_is_rejected() {
        let src = MINIMAL.replace(
            "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0=",
            "change-me-change-me-change-me-change-me",
        );
        assert!(
            Config::from_yaml(&src).is_err(),
            "a placeholder secret must not boot"
        );
    }

    #[test]
    fn a_partial_section_keeps_its_siblings_defaulted() {
        // The container case: the orchestrator sets exactly one server field
        // via OAG_SERVER__ADMIN_ADDR and expects the rest to default. Without
        // struct-level `default`, serde demands every sibling and the override
        // mechanism is useless.
        let src = format!("{MINIMAL}\nserver:\n  admin_addr: \"0.0.0.0:9091\"\n");
        let cfg = Config::from_yaml(&src).expect("a partial section should parse");
        assert_eq!(cfg.server.admin_addr, "0.0.0.0:9091");
        assert_eq!(
            cfg.server.public_addr, "0.0.0.0:8080",
            "sibling kept its default"
        );
        assert_eq!(cfg.server.max_body_bytes, 32 * 1024 * 1024);
    }

    #[test]
    fn the_default_body_limit_is_survivable_at_the_deployed_memory_limit() {
        // It was 256 MiB, buffered per in-flight request before anything was
        // parsed, against the 1 Gi the Helm chart asks for — so four concurrent
        // large POSTs was an OOM, and until authentication moved in front of
        // the body extractor they did not have to be authenticated ones.
        //
        // The bound is deliberately generous rather than an equality: the point
        // is the order of magnitude, not this exact number.
        let cfg = Config::from_yaml(MINIMAL).expect("parses");
        assert!(
            cfg.server.max_body_bytes <= 64 * 1024 * 1024,
            "a per-request buffer of {} bytes does not fit a 1 Gi replica under concurrency",
            cfg.server.max_body_bytes
        );
        // And still large enough for the payloads this gateway exists to carry.
        assert!(cfg.server.max_body_bytes >= 8 * 1024 * 1024);
    }

    #[test]
    fn the_connection_deadlines_default_to_something_the_server_can_apply() {
        // Both were config and documentation with nothing reading them until
        // `oag_server::listen` replaced `axum::serve`. A zero here would mean
        // "disabled", which is not what a slowloris mitigation should default
        // to.
        let cfg = Config::from_yaml(MINIMAL).expect("parses");
        assert!(!cfg.server.header_read_timeout.is_zero());
        assert!(!cfg.server.idle_timeout.is_zero());
    }

    #[test]
    fn two_listeners_is_the_default() {
        // Single-port mode is for platforms that force it, not a convenience.
        let cfg = Config::from_yaml(MINIMAL).expect("parses");
        assert!(!cfg.server.single_listener);
    }

    #[test]
    fn the_catalog_refresh_interval_defaults_to_something_useful() {
        // Zero disables it, which is a legitimate choice for a single-replica
        // deployment that restarts on every change — but it must not be the
        // default, or seeding a catalog appears to do nothing.
        let cfg = Config::from_yaml(MINIMAL).expect("parses");
        assert!(!cfg.gateway.catalog_refresh_interval.is_zero());
        assert!(cfg.gateway.catalog_refresh_interval <= Duration::from_mins(5));
    }

    #[test]
    fn provider_base_urls_are_optional_and_overridable() {
        let cfg = Config::from_yaml(MINIMAL).expect("parses");
        assert!(cfg.gateway.provider_base_urls.is_empty());

        let src = format!(
            "{MINIMAL}\ngateway:\n  provider_base_urls:\n    anthropic: \"http://127.0.0.1:9\"\n"
        );
        let cfg = Config::from_yaml(&src).expect("parses");
        assert_eq!(
            cfg.gateway
                .provider_base_urls
                .get("anthropic")
                .map(String::as_str),
            Some("http://127.0.0.1:9")
        );
    }

    #[test]
    fn a_partial_gateway_section_keeps_the_stream_invariant() {
        let src = format!("{MINIMAL}\ngateway:\n  same_account_retries: 5\n");
        let cfg = Config::from_yaml(&src).expect("parses");
        assert_eq!(cfg.gateway.same_account_retries, 5);
        assert!(cfg.gateway.max_stream_duration > cfg.gateway.stream_idle_timeout);
    }

    #[test]
    fn stream_ceiling_must_exceed_idle_timeout() {
        let src = format!(
            "{MINIMAL}\ngateway:\n  stream_idle_timeout: 600\n  stream_keepalive_interval: 10\n  max_stream_duration: 300\n  same_account_retries: 2\n  max_account_switches: 3\n"
        );
        assert!(Config::from_yaml(&src).is_err());
    }

    #[test]
    fn an_otlp_endpoint_is_refused_rather_than_ignored() {
        // Setting a config option that this build cannot honour must fail at
        // startup. Silently ignoring it means discovering the gap later, while
        // looking for traces that were never going to exist.
        let src = format!("{MINIMAL}\ntelemetry:\n  otlp_endpoint: \"http://collector:4317\"\n");
        let err = Config::from_yaml(&src).expect_err("must refuse");
        assert!(
            err.to_string().contains("no OTLP exporter"),
            "the error should say why, got: {err}"
        );
    }

    #[test]
    fn a_wrong_length_kek_is_caught_at_config_load_not_at_boot() {
        // 34 bytes rather than 32. Before this check, `migrate` accepted it —
        // it never builds a Kek — and only `serve` failed, as a crash loop with
        // the reason inside a restarting container. That is how it was found.
        let src = MINIMAL.replace(
            "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
            "dmVyaWZ5LW9ubHkta2VrLTMyLWJ5dGVzLTAxMjM0NTY3OA==",
        );
        let err = Config::from_yaml(&src).expect_err("must refuse");
        assert!(
            err.to_string().contains("32 bytes"),
            "the error should say what is wrong, got: {err}"
        );
    }

    #[test]
    fn a_kek_that_is_not_base64_is_caught_too() {
        let src = MINIMAL.replace(
            "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
            "not-base64-at-all!!!",
        );
        assert!(Config::from_yaml(&src).is_err());
    }
}
