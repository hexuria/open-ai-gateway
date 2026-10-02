//! Where the gateway may be told to send a request.
//!
//! Two URLs an operator writes become places this process connects to: a
//! capability service's health check and an endpoint's base URL. Both are
//! refused if they name a link-local or cloud-metadata address, as a literal
//! (`oag_core::catalog_url`, and here again) or once the name is resolved
//! (here, because resolving is I/O and `oag-core` does none).

use oag_core::endpoint::endpoint_base_url;
use oag_core::ip_is_denied;
use oag_core::provider::Platform;
use std::net::{IpAddr, ToSocketAddrs};
use url::{Host, Url};

/// Whether an endpoint on `platform` may be written with `raw` as its base
/// URL: every rule the reload applies ([`endpoint_base_url`]: a base URL at
/// all, no link-local or metadata literal, not a host
/// [`oag_core::endpoint::PLAIN_REFUSED_HOSTS`] keeps from a plain endpoint),
/// and then what the name resolves to now.
///
/// For the paths that write an endpoint. The reload does not call this: it
/// applies the same rules without the DNS step, so a resolver cannot decide
/// which endpoints serve. Loopback and private addresses pass both, which is
/// what lets a model server on the operator's own network be registered.
pub async fn validate_endpoint_base_url(raw: &str, platform: Platform) -> Result<(), String> {
    let normalised = endpoint_base_url(raw, platform).map_err(|refusal| refusal.message)?;
    let url = Url::parse(&normalised).map_err(|e| format!("not a URL: {e}"))?;
    deny_resolved_target(&url).await
}

pub(crate) async fn deny_resolved_target(url: &Url) -> Result<(), String> {
    // From the parsed host rather than `host_str`, which spells an IPv6
    // address in brackets: `[::1]` is not an `IpAddr`, so it went to DNS and
    // failed there.
    let host = match url.host() {
        Some(Host::Domain(name)) => name,
        // An address needs no lookup, and is judged as a resolved one is.
        // `catalog_url` judges it too where an endpoint is written, but this
        // answers for whatever URL it is handed, and an address it waved
        // through unexamined would be one only that caller had checked.
        Some(Host::Ipv4(ip)) => return judged(IpAddr::V4(ip), url),
        Some(Host::Ipv6(ip)) => return judged(IpAddr::V6(ip), url),
        None => return Err("URL is missing a host".to_owned()),
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let lookup = host.to_owned();
    let resolved = tokio::task::spawn_blocking(move || (lookup.as_str(), port).to_socket_addrs())
        .await
        .map_err(|e| format!("resolving host: {e}"))?
        .map_err(|e| format!("resolving {host}: {e}"))?;

    let mut any = false;
    for addr in resolved {
        any = true;
        if ip_is_denied(addr.ip()) {
            return Err(format!(
                "refusing to probe {host}: it resolves to a link-local or metadata address"
            ));
        }
    }
    if !any {
        return Err(format!("resolving {host}: no addresses"));
    }
    Ok(())
}

/// `ip`, the address `url` names, refused if [`ip_is_denied`] says so.
fn judged(ip: IpAddr, url: &Url) -> Result<(), String> {
    if ip_is_denied(ip) {
        return Err(format!(
            "refusing to probe {}: it is a link-local or metadata address",
            url.host_str().unwrap_or_default()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_endpoint_base_url_is_refused_for_what_it_says_before_any_lookup() {
        for (raw, says) in [
            (
                "http://169.254.169.254/latest",
                "link-local or cloud-metadata",
            ),
            ("http://[fe80::1]:8000", "link-local or cloud-metadata"),
            (
                "http://metadata.google.internal/v1",
                "link-local or cloud-metadata",
            ),
            (
                "http://metadata.google.internal./v1",
                "link-local or cloud-metadata",
            ),
            ("https://api.openai.com/v1", "openai.com"),
            ("https://API.Anthropic.com.:443", "anthropic.com"),
            ("https://h.example/v1?key=1", "contains '?'"),
            ("file:///etc/passwd", "http or https"),
        ] {
            let err = validate_endpoint_base_url(raw, Platform::Plain)
                .await
                .expect_err(raw);
            assert!(err.contains(says), "{raw}: {err}");
        }
    }

    #[tokio::test]
    async fn a_local_model_server_can_be_registered() {
        // Addresses need no lookup, and `localhost` resolves from the hosts
        // file: nothing here leaves the machine.
        for raw in [
            "http://127.0.0.1:8000/v1",
            "http://[::1]:11434",
            "http://10.1.2.3:8000/v1",
            "http://192.168.1.20/v1/",
            "http://localhost:8000/v1",
        ] {
            validate_endpoint_base_url(raw, Platform::Plain)
                .await
                .unwrap_or_else(|e| panic!("{raw}: {e}"));
        }
    }

    /// An address in the URL is judged as a resolved one is, with no lookup:
    /// whoever calls this with a URL `catalog_url` never saw is refused the
    /// same addresses.
    #[tokio::test]
    async fn an_address_is_judged_without_a_lookup() {
        for raw in [
            "http://169.254.169.254/latest",
            "http://[fe80::1]:8000/",
            "http://[fd00:ec2::254]/latest/meta-data/",
            "http://[fd20:ce::254]/computeMetadata/v1/",
            "http://100.100.100.200/latest/meta-data/",
            "http://[::ffff:169.254.169.254]/",
        ] {
            let url = Url::parse(raw).expect("a URL");
            let err = deny_resolved_target(&url).await.expect_err(raw);
            assert!(
                err.contains("is a link-local or metadata address"),
                "{raw}: {err}"
            );
        }
        for raw in [
            "http://127.0.0.1:8000/v1",
            "http://[::1]:11434",
            "http://10.1.2.3:8000/v1",
            "http://[fd12:3456::7]:8000/v1",
        ] {
            let url = Url::parse(raw).expect("a URL");
            deny_resolved_target(&url)
                .await
                .unwrap_or_else(|e| panic!("{raw}: {e}"));
        }
        // And a base URL naming one is refused before anything is asked.
        for raw in [
            "http://[fd00:ec2::254]/v1",
            "http://[fd20:ce::254]/v1",
            "http://100.100.100.200/v1",
        ] {
            let err = validate_endpoint_base_url(raw, Platform::Plain)
                .await
                .expect_err(raw);
            assert!(err.contains("link-local or cloud-metadata"), "{raw}: {err}");
        }
    }

    #[tokio::test]
    async fn a_name_that_passes_the_rules_is_then_resolved() {
        // RFC 6761 reserves `.invalid`, so nothing answers for it: refused by
        // the lookup, which is the proof the lookup ran.
        let err = validate_endpoint_base_url("https://no-such-host.invalid/v1", Platform::Plain)
            .await
            .expect_err("nothing answers for .invalid");
        assert!(err.starts_with("resolving no-such-host.invalid"), "{err}");
    }
}
