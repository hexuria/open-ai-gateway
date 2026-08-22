//! Typed configuration.
//!
//! Static configuration is a file plus environment overrides. Runtime-tunable
//! settings live in the `setting` table. sub2api put nearly everything in a
//! generic key-value table and grew a 2466-line handler parsing it; the split
//! here is "does changing this require a restart" — if no, it is a setting; if
//! yes, it belongs in the file where it can be reviewed in a pull request.

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
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Inference traffic. This is the only listener the load balancer fronts.
    pub public_addr: String,
    /// Admin API, the SPA, `/metrics`, `/health/ready`. Bind this to the
    /// internal network. sub2api served admin and inference on one port, which
    /// means every admin endpoint inherits the public listener's exposure.
    pub admin_addr: String,
    /// How long a client may take to send its request headers.
    #[serde(with = "humantime_secs")]
    pub header_read_timeout: Duration,
    /// Idle keep-alive timeout. Deliberately *not* a whole-response write
    /// timeout: a streamed completion legitimately runs for many minutes and
    /// any total-response deadline will sever it.
    #[serde(with = "humantime_secs")]
    pub idle_timeout: Duration,
    /// Maximum inbound body. Large because image and document payloads are.
    pub max_body_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            public_addr: "0.0.0.0:8080".to_owned(),
            admin_addr: "127.0.0.1:8081".to_owned(),
            header_read_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_mins(2),
            max_body_bytes: 256 * 1024 * 1024,
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
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            stream_idle_timeout: Duration::from_mins(3),
            stream_keepalive_interval: Duration::from_secs(10),
            max_stream_duration: Duration::from_mins(30),
            same_account_retries: 2,
            max_account_switches: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// `tracing` filter, e.g. `info,oag_server=debug`.
    pub log_filter: String,
    /// JSON lines rather than human-readable. On in production.
    pub log_json: bool,
    /// OTLP endpoint. Tracing is off entirely when this is unset.
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
        if self.gateway.max_stream_duration <= self.gateway.stream_idle_timeout {
            return Err(crate::Error::Config(
                "gateway.max_stream_duration must exceed gateway.stream_idle_timeout"
                    .to_owned(),
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
    fn stream_ceiling_must_exceed_idle_timeout() {
        let src = format!("{MINIMAL}\ngateway:\n  stream_idle_timeout: 600\n  stream_keepalive_interval: 10\n  max_stream_duration: 300\n  same_account_retries: 2\n  max_account_switches: 3\n");
        assert!(Config::from_yaml(&src).is_err());
    }
}
