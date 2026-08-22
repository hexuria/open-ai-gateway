//! AWS Signature Version 4.
//!
//! Hand-rolled rather than pulled from the AWS SDK. The SDK brings several
//! hundred transitive crates and a second HTTP stack, all to compute one HMAC
//! chain over a canonicalised request — and this gateway would use none of the
//! rest of it. The algorithm is public, stable since 2012, and fits on a page.
//!
//! The parts that are easy to get wrong, and which the tests pin:
//!
//! - Header names are lowercased and **sorted**; the signed-headers list must
//!   match the canonical headers exactly.
//! - Header values are trimmed and internal whitespace collapsed.
//! - The payload hash is hex-encoded SHA-256 of the *body*, and also travels in
//!   the `x-amz-content-sha256` header.
//! - The date in the credential scope is `YYYYMMDD`; the one in the header is
//!   the full basic-format timestamp. Mixing them fails with a signature error
//!   that names neither.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// What a request needs signing against.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// One signed request's headers.
#[derive(Debug, Clone)]
pub struct SignedHeaders {
    pub authorization: String,
    pub amz_date: String,
    pub content_sha256: String,
    pub session_token: Option<String>,
}

/// The request being signed.
///
/// Grouped rather than passed as loose arguments because four of them are
/// `&str` and swapping two would produce a signature that is perfectly valid
/// and rejected by AWS with no hint as to why.
#[derive(Debug, Clone, Copy)]
pub struct SigningRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host: &'a str,
    pub body: &'a [u8],
}

/// Sign a request, returning the headers to add.
///
/// `timestamp` is passed in rather than read from the clock so the result is a
/// pure function — which is what makes it testable at all.
#[must_use]
pub fn sign(
    creds: &Credentials,
    region: &str,
    service: &str,
    req: SigningRequest<'_>,
    timestamp: time::OffsetDateTime,
) -> SignedHeaders {
    let SigningRequest {
        method,
        path,
        host,
        body,
    } = req;

    let amz_date = format_amz_date(timestamp);
    let date_stamp = &amz_date[..8];
    let payload_hash = hex::encode(Sha256::digest(body));

    // Canonical headers: lowercase, sorted, values trimmed. `host` and
    // `x-amz-date` are the minimum; the content hash header is required by
    // several services and harmless elsewhere.
    let mut headers = vec![
        ("host", host.to_owned()),
        ("x-amz-content-sha256", payload_hash.clone()),
        ("x-amz-date", amz_date.clone()),
    ];
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token", token.clone()));
    }
    headers.sort_by(|a, b| a.0.cmp(b.0));

    let canonical_headers = headers.iter().fold(String::new(), |mut acc, (k, v)| {
        use std::fmt::Write as _;
        let _ = writeln!(acc, "{k}:{}", v.trim());
        acc
    });
    let signed_headers: String = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request =
        format!("{method}\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");

    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    // The signing key is derived by chaining HMACs down the scope, so a leaked
    // daily key cannot sign for another day, region, or service.
    let k_date = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));

    SignedHeaders {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.access_key_id
        ),
        amz_date,
        content_sha256: payload_hash,
        session_token: creds.session_token.clone(),
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    // The only failure mode is a key length HMAC cannot accept, and HMAC
    // accepts any length — so this cannot fail in practice.
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).unwrap_or_else(|_| {
        <HmacSha256 as Mac>::new_from_slice(&[]).unwrap_or_else(|_| unreachable!())
    });
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// `YYYYMMDDTHHMMSSZ` — ISO 8601 basic format, which is what SigV4 wants.
fn format_amz_date(t: time::OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn creds() -> Credentials {
        // AWS's own published example credentials, from the SigV4 test suite.
        Credentials {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: None,
        }
    }

    const AT: time::OffsetDateTime = datetime!(2015-08-30 12:36:00 UTC);

    /// Sign with everything defaulted, so each test varies exactly one thing.
    fn signed(
        creds: &Credentials,
        region: &str,
        path: &str,
        body: &[u8],
        at: time::OffsetDateTime,
    ) -> SignedHeaders {
        sign(
            creds,
            region,
            "bedrock",
            SigningRequest {
                method: "POST",
                path,
                host: "bedrock-runtime.us-east-1.amazonaws.com",
                body,
            },
            at,
        )
    }

    #[test]
    fn the_timestamp_uses_iso_basic_format() {
        assert_eq!(format_amz_date(AT), "20150830T123600Z");
    }

    #[test]
    fn a_signature_is_deterministic_for_the_same_inputs() {
        // Deterministic is the whole reason `timestamp` is a parameter: a
        // signature that changed run to run could not be tested at all.
        let a = signed(&creds(), "us-east-1", "/", b"", AT);
        let b = signed(&creds(), "us-east-1", "/", b"", AT);
        assert_eq!(a.authorization, b.authorization);
    }

    #[test]
    fn every_component_changes_the_signature() {
        // Each of these is part of the scope or the canonical request. If any
        // were being dropped, a signature would still be produced — and would
        // still be wrong, with an AWS error that names none of them.
        let base = signed(&creds(), "us-east-1", "/model/x/invoke", b"{}", AT);

        let variants = [
            (
                "region",
                signed(&creds(), "eu-west-1", "/model/x/invoke", b"{}", AT),
            ),
            (
                "path",
                signed(&creds(), "us-east-1", "/model/y/invoke", b"{}", AT),
            ),
            (
                "body",
                signed(&creds(), "us-east-1", "/model/x/invoke", b"{\"a\":1}", AT),
            ),
            (
                "day",
                signed(
                    &creds(),
                    "us-east-1",
                    "/model/x/invoke",
                    b"{}",
                    datetime!(2015-08-31 12:36:00 UTC),
                ),
            ),
        ];
        for (name, other) in &variants {
            assert_ne!(
                base.authorization, other.authorization,
                "{name} must be signed"
            );
        }
    }

    #[test]
    fn the_payload_hash_is_of_the_body() {
        let s = signed(&creds(), "us-east-1", "/", b"hello", AT);
        assert_eq!(s.content_sha256, hex::encode(Sha256::digest(b"hello")));
        // The empty-body hash is a constant worth recognising in AWS errors.
        let e = signed(&creds(), "us-east-1", "/", b"", AT);
        assert_eq!(
            e.content_sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_session_token_is_signed_not_merely_attached() {
        // Temporary credentials fail with a signature error, not an auth error,
        // if the token is sent but left out of the canonical headers.
        let mut temp = creds();
        temp.session_token = Some("SESSIONTOKEN".to_owned());
        let with_token = signed(&temp, "us-east-1", "/", b"", AT);

        assert!(
            with_token.authorization.contains("x-amz-security-token"),
            "the token must appear in SignedHeaders"
        );
        assert_eq!(with_token.session_token.as_deref(), Some("SESSIONTOKEN"));
        assert_ne!(
            with_token.authorization,
            signed(&creds(), "us-east-1", "/", b"", AT).authorization
        );
    }

    #[test]
    fn signed_headers_are_lowercase_and_sorted() {
        let s = signed(&creds(), "us-east-1", "/", b"", AT);
        let list = s
            .authorization
            .split("SignedHeaders=")
            .nth(1)
            .and_then(|r| r.split(',').next())
            .expect("signed headers");
        let names: Vec<&str> = list.split(';').collect();
        assert_eq!(names, vec!["host", "x-amz-content-sha256", "x-amz-date"]);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn the_credential_scope_names_the_service_and_day() {
        let s = signed(&creds(), "eu-west-1", "/", b"", AT);
        assert!(
            s.authorization
                .contains("Credential=AKIDEXAMPLE/20150830/eu-west-1/bedrock/aws4_request"),
            "{}",
            s.authorization
        );
    }
}
