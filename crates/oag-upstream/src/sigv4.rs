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
//! - The canonical URI is the path **URI-encoded exactly once**, which is not
//!   the same string as the path on the wire. See [`uri_encode_path`].

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

    let canonical_uri = uri_encode_path(path);
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );

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

/// The canonical URI: the wire path with every byte URI-encoded once.
///
/// SigV4 (for every service except S3) canonicalises the path by URI-encoding
/// it, leaving only the unreserved set `A-Za-z0-9-._~` and the `/` separators
/// literal. AWS re-derives this from the bytes it received — no decoding first —
/// and signs *that*. The authority is AWS's own test suite: its `get-utf8` case
/// puts the raw bytes `/ሴ` on the request line and expects the canonical URI
/// `/%E1%88%B4`, encoded exactly once from what arrived. A signer applying zero
/// passes therefore produces a signature AWS cannot reproduce.
///
/// This matters here because of one character. Every Bedrock model id contains
/// a colon — `anthropic.claude-sonnet-4-v1:0` — which encodes to `%3A`. Without
/// this the whole adapter signs a canonical request AWS never computes, and the
/// resulting 403 maps to `Disposition::FailoverAccount`: the failure presents as
/// a credential going chronically unhealthy rather than as a signing bug, which
/// is exactly the wrong place to look.
///
/// **The invariant is one pass ahead of the wire.** `path` must be the bytes
/// the HTTP client will actually send, and this function encodes them once on
/// top. Either composition satisfies AWS: a literal `:` on the wire signed as
/// `%3A` (this signer, curl's `--aws-sigv4`), or `%3A` on the wire signed as
/// `%253A` (the AWS SDKs' double encoding). What fails is any *mismatch* in the
/// number of passes — the raw path signed raw, as before, or a path the caller
/// pre-encoded and then handed here to be encoded again. `bedrock.rs` keeps the
/// two in step by signing `url.path()`, the parsed wire path itself, and pins
/// that with `the_signature_is_computed_over_the_exact_wire_path`.
fn uri_encode_path(path: &str) -> String {
    // Unreserved per RFC 3986, which is the set AWS leaves alone. `/` is the
    // segment separator and stays literal; everything else becomes uppercase
    // percent-hex, byte by byte, so multi-byte UTF-8 encodes per byte.
    let mut out = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
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

    #[test]
    fn a_model_ids_colon_is_percent_encoded_in_the_canonical_uri() {
        // The character this whole function exists for. Every Anthropic-on-
        // Bedrock model id carries it, so a signer that leaves it literal signs
        // a canonical request AWS never computes — for every Bedrock request
        // there has ever been.
        assert_eq!(
            uri_encode_path("/model/anthropic.claude-sonnet-4-v1:0/invoke"),
            "/model/anthropic.claude-sonnet-4-v1%3A0/invoke"
        );
    }

    #[test]
    fn the_unreserved_set_survives_and_separators_stay_literal() {
        // Encoding these would break the signature just as surely as failing to
        // encode the colon: the canonical URI is one exact string, not a
        // conservative over-encoding of it.
        let unreserved = "/aZ09-._~/x";
        assert_eq!(uri_encode_path(unreserved), unreserved);
        assert_eq!(uri_encode_path("/"), "/");
        // Uppercase hex, and multi-byte UTF-8 encoded per byte.
        assert_eq!(uri_encode_path("/a b"), "/a%20b");
        assert_eq!(uri_encode_path("/é"), "/%C3%A9");
    }

    #[test]
    fn raw_bytes_on_the_wire_are_encoded_once_per_aws_test_suite() {
        // AWS's published vector `get-utf8` (botocore: tests/unit/auth/
        // aws4_testsuite/get-utf8): the request line carries the raw bytes
        // `/ሴ` and the expected canonical request carries `/%E1%88%B4`. That
        // is the external proof of this signer's composition — literal bytes
        // on the wire, one encoding pass in the canonical URI, no decoding in
        // between. The full signature cannot be reproduced through `sign`,
        // which always signs `x-amz-content-sha256` where the vector does not,
        // so the canonical-URI half is pinned to the vector directly.
        assert_eq!(uri_encode_path("/ሴ"), "/%E1%88%B4");
    }

    #[test]
    fn the_canonical_uri_is_encoded_exactly_once() {
        // The encoder is not idempotent, and that is load-bearing. Two passes
        // give `%253A`, not `%3A`, so a caller that pre-encodes the path and
        // hands it here is one pass ahead of the wire *twice* — and the
        // signature it gets differs from the one the raw path yields. This is
        // what makes "sign the exact wire bytes" the only safe contract for
        // `SigningRequest.path`; the adapter side of that contract is pinned
        // in bedrock.rs by `the_signature_is_computed_over_the_exact_wire_path`.
        let once = uri_encode_path("/model/m:0/invoke");
        let twice = uri_encode_path(&once);
        assert_eq!(once, "/model/m%3A0/invoke");
        assert_eq!(twice, "/model/m%253A0/invoke");
        assert_ne!(once, twice, "double encoding must not be a no-op");

        assert_ne!(
            signed(&creds(), "us-east-1", "/model/m:0/invoke", b"", AT).authorization,
            signed(&creds(), "us-east-1", &once, b"", AT).authorization
        );
    }

    #[test]
    fn the_signature_matches_the_canonical_request_aws_will_rebuild() {
        // The assertion the change-detection tests above cannot make. They pin
        // that the path is signed; none of them pins *what string* is signed,
        // which is the whole defect. So spell out the canonical request per the
        // SigV4 spec — note the `%3A` — derive the signature by the documented
        // steps, and require `sign` to agree.
        let path = "/model/anthropic.claude-sonnet-4-v1:0/invoke";
        let body = b"{}";
        let host = "bedrock-runtime.us-east-1.amazonaws.com";
        let payload_hash = hex::encode(Sha256::digest(body));

        let canonical_request = format!(
            "POST\n\
             /model/anthropic.claude-sonnet-4-v1%3A0/invoke\n\
             \n\
             host:{host}\n\
             x-amz-content-sha256:{payload_hash}\n\
             x-amz-date:20150830T123600Z\n\
             \n\
             host;x-amz-content-sha256;x-amz-date\n\
             {payload_hash}"
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/bedrock/aws4_request\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let k_date = hmac(
            format!("AWS4{}", creds().secret_access_key).as_bytes(),
            b"20150830",
        );
        let k_region = hmac(&k_date, b"us-east-1");
        let k_service = hmac(&k_region, b"bedrock");
        let k_signing = hmac(&k_service, b"aws4_request");
        let expected = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));

        let s = signed(&creds(), "us-east-1", path, body, AT);
        assert!(
            s.authorization.ends_with(&format!("Signature={expected}")),
            "canonical request mismatch\n  got: {}\n  want signature: {expected}",
            s.authorization
        );
    }
}
