//! Capability-service catalog types.
//!
//! A row in this catalog is a pointer at a service that already exists
//! somewhere else — a sandbox, a tool host, a guard, a reducer. The gateway
//! does not implement any of those. It registers the URL, health-checks it,
//! and deep-links to that service's own dashboard.
//!
//! URL validation lives here because it is a pure function of a string, and
//! because "fail closed on anything that is not http(s)" is a domain rule,
//! not an HTTP-handler convenience.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use url::Url;

/// What kind of capability this row points at.
///
/// Closed on purpose: a free-text kind becomes a junk drawer, and the
/// dashboard then cannot group or filter without guessing. `other` is the
/// escape hatch for something that does not fit yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Sandbox,
    Tool,
    Guard,
    Reduce,
    Harness,
    Browser,
    Other,
}

impl ServiceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Tool => "tool",
            Self::Guard => "guard",
            Self::Reduce => "reduce",
            Self::Harness => "harness",
            Self::Browser => "browser",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for ServiceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ServiceKind {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sandbox" => Ok(Self::Sandbox),
            "tool" => Ok(Self::Tool),
            "guard" => Ok(Self::Guard),
            "reduce" => Ok(Self::Reduce),
            "harness" => Ok(Self::Harness),
            "browser" => Ok(Self::Browser),
            "other" => Ok(Self::Other),
            other => Err(crate::Error::Config(format!(
                "kind must be sandbox, tool, guard, reduce, harness, browser, or other, not '{other}'"
            ))),
        }
    }
}

/// Parse a catalog URL: http(s) only, no embedded credentials, no
/// link-local or cloud-metadata target.
///
/// Loopback and RFC1918 are allowed. Capability services live on the
/// organisation's own network; refusing those would make the catalog
/// unusable for the thing it exists to register. Link-local and the
/// well-known metadata hostnames are not org services — they are the
/// cheap SSRF footguns.
pub fn catalog_url(raw: &str) -> Result<Url, crate::Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(crate::Error::Config("URL is empty".to_owned()));
    }
    if trimmed.len() > 2048 {
        return Err(crate::Error::Config("URL is too long".to_owned()));
    }

    let url = Url::parse(trimmed).map_err(|e| crate::Error::Config(format!("not a URL: {e}")))?;

    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(crate::Error::Config(format!(
                "URL must be http or https, not '{other}'"
            )));
        }
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(crate::Error::Config(
            "URL must not contain credentials".to_owned(),
        ));
    }

    match url.host() {
        Some(url::Host::Ipv4(ip)) if ip_is_denied(IpAddr::V4(ip)) => {
            return Err(denied_target());
        }
        Some(url::Host::Ipv6(ip)) if ip_is_denied(IpAddr::V6(ip)) => {
            return Err(denied_target());
        }
        Some(url::Host::Domain(name)) if is_metadata_host(name) => {
            return Err(denied_target());
        }
        Some(_) => {}
        None => {
            return Err(crate::Error::Config("URL is missing a host".to_owned()));
        }
    }

    Ok(url)
}

/// Build the health-check URL from a registered base and path.
///
/// `health_path` must be a single absolute path (`/health`). A scheme, a
/// protocol-relative host, or a query string would let a row smuggle a
/// different target than `base_url` advertised.
pub fn health_url(base: &str, path: &str) -> Result<Url, crate::Error> {
    let path = path.trim();
    if path.is_empty() || path.len() > 256 {
        return Err(crate::Error::Config(
            "health_path must be a non-empty path of at most 256 characters".to_owned(),
        ));
    }
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(crate::Error::Config(
            "health_path must start with a single '/'".to_owned(),
        ));
    }
    if path.contains(['?', '#', '\\', '\r', '\n', '\0']) {
        return Err(crate::Error::Config(
            "health_path must be a path, not a URL".to_owned(),
        ));
    }

    let mut url = catalog_url(base)?;
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    // Host cannot change when only the path is set, but fail closed anyway
    // so a future `url` crate surprise cannot become an open redirect.
    catalog_url(url.as_str())?;
    Ok(url)
}

/// Addresses we will not send a health check to.
///
/// Link-local (including the cloud metadata well-known `169.254.169.254`),
/// unspecified, multicast, and broadcast. Not loopback and not RFC1918 —
/// those are where the catalog's own backends actually run.
#[must_use]
pub fn ip_is_denied(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_denied(v4),
        IpAddr::V6(v6) => v6_is_denied(v6),
    }
}

fn v4_is_denied(ip: Ipv4Addr) -> bool {
    ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast()
}

fn v6_is_denied(ip: Ipv6Addr) -> bool {
    if ip.is_unicast_link_local() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    ip.to_ipv4_mapped().is_some_and(v4_is_denied)
}

fn is_metadata_host(name: &str) -> bool {
    // Compared case-insensitively; DNS is.
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "metadata" | "metadata.google.internal" | "metadata.google.com" | "metadata.azure.com"
    )
}

fn denied_target() -> crate::Error {
    crate::Error::Config("refusing a link-local or cloud-metadata URL".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_on_the_wire() {
        for kind in [
            ServiceKind::Sandbox,
            ServiceKind::Tool,
            ServiceKind::Guard,
            ServiceKind::Reduce,
            ServiceKind::Harness,
            ServiceKind::Browser,
            ServiceKind::Other,
        ] {
            assert_eq!(kind.as_str().parse::<ServiceKind>().unwrap(), kind);
        }
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        assert!(matches!(
            "sandboxx".parse::<ServiceKind>(),
            Err(crate::Error::Config(_))
        ));
    }

    #[test]
    fn https_org_urls_are_accepted() {
        let url = catalog_url("https://orgo.example.invalid/").unwrap();
        assert_eq!(url.host_str(), Some("orgo.example.invalid"));
    }

    #[test]
    fn loopback_is_allowed_so_a_local_service_can_be_registered() {
        catalog_url("http://127.0.0.1:9090/").unwrap();
        catalog_url("http://localhost:9090/health").unwrap();
    }

    #[test]
    fn rfc1918_is_allowed() {
        catalog_url("http://10.1.2.3:8080/").unwrap();
        catalog_url("http://192.168.1.10/").unwrap();
    }

    #[test]
    fn schemes_other_than_http_are_refused() {
        for raw in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "gopher://example.invalid/",
            "ftp://example.invalid/",
        ] {
            let err = catalog_url(raw).unwrap_err().to_string();
            assert!(
                err.contains("http or https"),
                "{raw} should fail closed on scheme, got {err}"
            );
        }
    }

    #[test]
    fn embedded_credentials_are_refused() {
        let err = catalog_url("https://user:secret@orgo.example.invalid/")
            .unwrap_err()
            .to_string();
        assert!(err.contains("credentials"), "{err}");
    }

    #[test]
    fn link_local_and_metadata_literals_are_refused() {
        for raw in [
            "http://169.254.169.254/latest/meta-data",
            "http://169.254.1.1/",
            "http://[fe80::1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://0.0.0.0/",
            "http://255.255.255.255/",
        ] {
            let err = catalog_url(raw).unwrap_err().to_string();
            assert!(
                err.contains("link-local") || err.contains("metadata"),
                "{raw} should be denied, got {err}"
            );
        }
    }

    #[test]
    fn well_known_metadata_hostnames_are_refused() {
        for raw in [
            "http://metadata.google.internal/",
            "http://METADATA.google.internal/computeMetadata/v1",
            "http://metadata/",
        ] {
            assert!(
                catalog_url(raw).is_err(),
                "{raw} is a metadata hostname and must fail closed"
            );
        }
    }

    #[test]
    fn health_path_cannot_smuggle_a_different_host() {
        assert!(health_url("https://orgo.example.invalid", "//evil.example/x").is_err());
        assert!(health_url("https://orgo.example.invalid", "https://evil.example/").is_err());
        assert!(health_url("https://orgo.example.invalid", "/health?x=1").is_err());
        assert!(health_url("https://orgo.example.invalid", "health").is_err());
    }

    #[test]
    fn health_url_joins_path_onto_the_registered_host() {
        let url = health_url("https://orgo.example.invalid/api", "/ready").unwrap();
        assert_eq!(url.as_str(), "https://orgo.example.invalid/ready");
        assert_eq!(url.host_str(), Some("orgo.example.invalid"));
    }

    #[test]
    fn empty_and_blank_urls_fail_closed() {
        assert!(catalog_url("").is_err());
        assert!(catalog_url("   ").is_err());
    }
}
