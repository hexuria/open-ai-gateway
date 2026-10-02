//! The mint, against a stand-in token endpoint.
//!
//! The key under `tests/fixtures` is a test-only RSA key. It was generated for
//! these tests and has never been attached to a Google service account.

use super::*;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicI64, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Test-only; see `tests/fixtures/README.md`.
const TEST_KEY: &str = include_str!("../../tests/fixtures/gcp-test-key.pem");
/// The test key's public half, which is what a signature is checked against.
const TEST_PUBLIC_KEY: &str = include_str!("../../tests/fixtures/gcp-test-key.rsa-public.pem");

const EMAIL: &str = "vertex-caller@oag-test.invalid";
const KEY_ID: &str = "0123456789abcdef0123456789abcdef01234567";

/// Any fixed instant. The tests only ever move relative to it.
const T0: i64 = 1_790_000_000;

fn fixed_clock() -> i64 {
    T0
}

/// A service-account key shaped as Google issues one, around the test key.
fn key_json() -> Value {
    json!({
        "type": "service_account",
        "project_id": "oag-test",
        "private_key_id": KEY_ID,
        "private_key": TEST_KEY,
        "client_email": EMAIL,
        "client_id": "100000000000000000001",
        "auth_uri": "https://accounts.google.com/o/oauth2/auth",
        "token_uri": "https://oauth2.googleapis.com/token",
        "universe_domain": "googleapis.com",
    })
}

/// Google's answer to a grant it accepts.
fn granted(token: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": token,
        "expires_in": 3600,
        "token_type": "Bearer",
    }))
}

/// A stand-in token endpoint at `/token` that answers every grant with
/// `response`, and fails its `verify` unless it saw exactly `grants` of them.
async fn token_endpoint(response: impl Respond + 'static, grants: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(response)
        .expect(grants)
        .mount(&server)
        .await;
    server
}

fn token_url(server: &MockServer) -> String {
    format!("{}/token", server.uri())
}

fn cache(server: &MockServer) -> GcpTokenCache {
    GcpTokenCache::new(token_url(server))
        .expect("a client")
        .with_clock(fixed_clock)
}

/// The form of every grant `server` received, decoded as the endpoint reads it.
async fn grants(server: &MockServer) -> Vec<HashMap<String, String>> {
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .iter()
        .map(|r| url::form_urlencoded::parse(&r.body).into_owned().collect())
        .collect()
}

/// One base64url segment of a JWT, as JSON.
fn segment(part: &str) -> Value {
    let bytes = URL_SAFE_NO_PAD.decode(part).expect("base64url");
    serde_json::from_slice(&bytes).expect("a JSON segment")
}

fn claims(grant: &HashMap<String, String>) -> Value {
    segment(
        grant["assertion"]
            .split('.')
            .nth(1)
            .expect("a claims segment"),
    )
}

/// The DER in a PEM block. Written out here rather than borrowed from
/// `pkcs8_der`, so the test does not check the code with the code.
fn pem_der(pem: &str, label: &str) -> Vec<u8> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let body: String = pem
        .lines()
        .skip_while(|line| *line != begin)
        .skip(1)
        .take_while(|line| *line != end)
        .collect();
    STANDARD.decode(body).expect("a base64 PEM body")
}

/// Fail if `shown` holds any of `secrets`, or any line of the private key.
fn assert_quotes_nothing(shown: &str, secrets: &[&str]) {
    for secret in secrets {
        assert!(!secret.is_empty(), "an empty secret proves nothing");
        assert!(!shown.contains(secret), "a secret is quoted in: {shown}");
    }
    let key_lines: Vec<&str> = TEST_KEY
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    assert!(key_lines.len() > 20, "the scan reads the key's body");
    for line in key_lines {
        assert!(
            !shown.contains(line),
            "a line of the private key is quoted in: {shown}"
        );
    }
}

/// The grant is RFC 7523's, and its assertion is a JWT the test key signed,
/// naming the key, the account, the scope, the audience and one hour.
#[tokio::test]
async fn a_mint_posts_a_jwt_bearer_grant_signed_by_the_key() {
    let server = token_endpoint(granted("ya29.minted"), 1).await;
    let token = cache(&server)
        .token(AccountId::new(), &key_json().to_string(), None)
        .await
        .expect("a token");
    assert_eq!(token, "ya29.minted");

    let grants = grants(&server).await;
    let [grant] = grants.as_slice() else {
        panic!("one grant, not {}", grants.len());
    };
    assert_eq!(
        grant["grant_type"],
        "urn:ietf:params:oauth:grant-type:jwt-bearer"
    );
    let assertion: Vec<&str> = grant["assertion"].split('.').collect();
    let [header, payload, signature] = assertion.as_slice() else {
        panic!("a three-part JWT, not {} parts", assertion.len());
    };

    assert_eq!(
        segment(header),
        json!({ "alg": "RS256", "typ": "JWT", "kid": KEY_ID })
    );
    let claims = segment(payload);
    assert_eq!(
        claims,
        json!({
            "iss": EMAIL,
            "scope": "https://www.googleapis.com/auth/cloud-platform",
            "aud": "https://oauth2.googleapis.com/token",
            "iat": T0 - 30,
            "exp": T0 - 30 + 3600,
        })
    );
    assert_eq!(
        claims["exp"].as_i64().expect("exp") - claims["iat"].as_i64().expect("iat"),
        3600,
        "Google refuses an assertion valid for longer than an hour"
    );

    // Signed by the key whose public half is committed beside it.
    let public_key = UnparsedPublicKey::new(
        &RSA_PKCS1_2048_8192_SHA256,
        pem_der(TEST_PUBLIC_KEY, "RSA PUBLIC KEY"),
    );
    let signature = URL_SAFE_NO_PAD.decode(signature).expect("base64url");
    public_key
        .verify(format!("{header}.{payload}").as_bytes(), &signature)
        .expect("the test key's signature over header and claims");
    // And the check can fail: the same signature does not cover other claims.
    let forged = URL_SAFE_NO_PAD.encode(json!({ "iss": "someone-else" }).to_string());
    assert!(
        public_key
            .verify(format!("{header}.{forged}").as_bytes(), &signature)
            .is_err()
    );
}

#[tokio::test]
async fn a_second_call_within_validity_reuses_the_token() {
    let server = token_endpoint(granted("ya29.first"), 1).await;
    let cache = cache(&server);
    let (account, key) = (AccountId::new(), key_json().to_string());

    assert_eq!(
        cache.token(account, &key, None).await.expect("minted"),
        "ya29.first"
    );
    assert_eq!(
        cache.token(account, &key, None).await.expect("cached"),
        "ya29.first"
    );
    server.verify().await;
}

/// Granted for 3600 seconds at `T0`, a token is served until `T0 + 3300` and
/// not at it: the last five minutes are spent minting its replacement.
#[tokio::test]
async fn a_token_within_five_minutes_of_expiry_is_minted_again() {
    static NOW: AtomicI64 = AtomicI64::new(T0);
    fn now() -> i64 {
        NOW.load(Ordering::SeqCst)
    }

    let server = MockServer::start().await;
    for (token, once) in [("ya29.first", true), ("ya29.second", false)] {
        let mock = Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(granted(token))
            .expect(1);
        let mock = if once { mock.up_to_n_times(1) } else { mock };
        mock.mount(&server).await;
    }
    let cache = GcpTokenCache::new(token_url(&server))
        .expect("a client")
        .with_clock(now);
    let (account, key) = (AccountId::new(), key_json().to_string());

    assert_eq!(
        cache.token(account, &key, None).await.expect("minted"),
        "ya29.first"
    );
    NOW.store(T0 + 3299, Ordering::SeqCst);
    assert_eq!(
        cache.token(account, &key, None).await.expect("cached"),
        "ya29.first"
    );
    NOW.store(T0 + 3300, Ordering::SeqCst);
    assert_eq!(
        cache
            .token(account, &key, None)
            .await
            .expect("minted again"),
        "ya29.second"
    );

    let grants = grants(&server).await;
    assert_eq!(grants.len(), 2);
    assert_eq!(
        claims(&grants[1])["iat"],
        T0 + 3300 - 30,
        "signed at the new time"
    );
    server.verify().await;
}

/// Ten callers arriving at once for one account wait for a single mint, and
/// all of them get its token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_for_one_account_share_one_mint() {
    const CALLERS: usize = 10;
    // Slow enough that every caller arrives while the first mint is in flight.
    let slow = granted("ya29.shared").set_delay(Duration::from_millis(300));
    let server = token_endpoint(slow, 1).await;
    let cache = Arc::new(cache(&server));
    let (account, key) = (AccountId::new(), key_json().to_string());
    let start = Arc::new(tokio::sync::Barrier::new(CALLERS));

    let callers: Vec<_> = (0..CALLERS)
        .map(|_| {
            let (cache, key, start) = (Arc::clone(&cache), key.clone(), Arc::clone(&start));
            tokio::spawn(async move {
                start.wait().await;
                cache.token(account, &key, None).await
            })
        })
        .collect();
    for caller in callers {
        let token = caller.await.expect("the caller ran").expect("a token");
        assert_eq!(token, "ya29.shared");
    }
    server.verify().await;
}

/// Accounts do not share tokens, even when they hold the same key.
#[tokio::test]
async fn each_account_gets_its_own_token() {
    let server = MockServer::start().await;
    for (token, once) in [("ya29.for-a", true), ("ya29.for-b", false)] {
        let mock = Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(granted(token))
            .expect(1);
        let mock = if once { mock.up_to_n_times(1) } else { mock };
        mock.mount(&server).await;
    }
    let cache = cache(&server);
    let (a, b, key) = (AccountId::new(), AccountId::new(), key_json().to_string());

    assert_eq!(cache.token(a, &key, None).await.expect("a"), "ya29.for-a");
    assert_eq!(cache.token(b, &key, None).await.expect("b"), "ya29.for-b");
    assert_eq!(
        cache.token(a, &key, None).await.expect("a again"),
        "ya29.for-a"
    );
    server.verify().await;
}

#[tokio::test]
async fn a_credential_that_is_not_a_service_account_key_is_refused() {
    let server = token_endpoint(granted("ya29.never"), 0).await;
    let mut key = key_json();
    key["type"] = json!("authorized_user");

    let err = cache(&server)
        .token(AccountId::new(), &key.to_string(), None)
        .await
        .expect_err("refused");
    assert!(
        matches!(err, Error::UpstreamUnavailable { lasting: true, .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("service_account"), "{err}");
    assert_quotes_nothing(&err.to_string(), &[]);
    server.verify().await;
}

/// Refused before anything is sent, and without quoting the key: serde quotes
/// the value it chokes on, which for a key pasted as one JSON string is the
/// whole key.
#[tokio::test]
async fn a_malformed_key_is_refused_without_being_quoted() {
    let server = token_endpoint(granted("ya29.never"), 0).await;
    let cache = cache(&server);
    let body: Vec<&str> = TEST_KEY
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let armoured =
        |body: &str| format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----\n");
    let with_private_key = |private_key: String| {
        let mut key = key_json();
        key["private_key"] = json!(private_key);
        key.to_string()
    };
    let without_private_key = {
        let mut key = key_json();
        key.as_object_mut()
            .expect("an object")
            .remove("private_key");
        key.to_string()
    };

    let cases = [
        ("no PEM armour", with_private_key(body.join("\n"))),
        (
            "PKCS#1 armour",
            with_private_key(TEST_KEY.replace("PRIVATE KEY", "RSA PRIVATE KEY")),
        ),
        (
            "a body that is not base64",
            with_private_key(armoured("not*base64!")),
        ),
        (
            "half a key",
            with_private_key(armoured(&body[..10].join("\n"))),
        ),
        ("no private_key", without_private_key),
        ("the key as a bare JSON string", json!(TEST_KEY).to_string()),
        ("a PEM where the JSON belongs", TEST_KEY.to_owned()),
    ];
    for (case, sa_json) in cases {
        let err = cache
            .token(AccountId::new(), &sa_json, None)
            .await
            .expect_err(case);
        assert!(
            matches!(err, Error::UpstreamUnavailable { lasting: true, .. }),
            "{case}: {err:?}"
        );
        assert_quotes_nothing(&err.to_string(), &[]);
    }
    server.verify().await;
}

/// Google's answer to a key it no longer honours is a credential error that
/// names the status and the OAuth code, and nothing that was sent.
#[tokio::test]
async fn a_refused_grant_is_an_error_that_names_the_code() {
    let refusal = ResponseTemplate::new(400).set_body_json(json!({
        "error": "invalid_grant",
        "error_description": "Invalid JWT Signature.",
    }));
    let server = token_endpoint(refusal, 1).await;

    let err = cache(&server)
        .token(AccountId::new(), &key_json().to_string(), None)
        .await
        .expect_err("refused");
    assert!(
        matches!(err, Error::UpstreamUnavailable { lasting: true, .. }),
        "Google refused the key: {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("400") && message.contains("(invalid_grant: Invalid JWT Signature.)"),
        "Google's own words for why: {message}"
    );
    let sent = grants(&server).await;
    assert_quotes_nothing(&message, &[sent[0]["assertion"].as_str()]);
    server.verify().await;
}

/// An endpoint that writes the whole grant back, assertion and all, into the
/// fields an OAuth error has. The status reaches the error; nothing the
/// endpoint wrote does.
#[tokio::test]
async fn a_refusal_that_echoes_the_assertion_does_not_get_it_into_the_error() {
    let echo = |request: &Request| {
        let sent = String::from_utf8_lossy(&request.body).into_owned();
        ResponseTemplate::new(401).set_body_json(json!({
            "error": sent,
            "error_description": sent,
        }))
    };
    let server = token_endpoint(echo, 1).await;

    let err = cache(&server)
        .token(AccountId::new(), &key_json().to_string(), None)
        .await
        .expect_err("refused");
    assert!(
        matches!(err, Error::UpstreamUnavailable { lasting: true, .. }),
        "{err:?}"
    );
    let message = err.to_string();
    assert!(message.contains("401"), "{message}");
    let sent = grants(&server).await;
    assert_quotes_nothing(&message, &[sent[0]["assertion"].as_str()]);
    server.verify().await;
}

/// Following a redirect would post the signed assertion to a host nobody
/// configured, so a 3xx fails the mint and the host it names hears nothing.
#[tokio::test]
async fn a_redirect_from_the_token_endpoint_is_not_followed() {
    let elsewhere = token_endpoint(granted("ya29.elsewhere"), 0).await;
    let redirect = ResponseTemplate::new(307).insert_header("location", token_url(&elsewhere));
    let server = token_endpoint(redirect, 1).await;

    let err = cache(&server)
        .token(AccountId::new(), &key_json().to_string(), None)
        .await
        .expect_err("not followed");
    assert!(
        matches!(err, Error::UpstreamUnavailable { lasting: false, .. }),
        "the way to Google, not the key: {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("307") && message.contains("not followed"),
        "{message}"
    );
    let sent = grants(&server).await;
    assert_quotes_nothing(&message, &[sent[0]["assertion"].as_str()]);
    elsewhere.verify().await;
    server.verify().await;
}

/// A 200 that is not a bearer token with a lifetime is a failed mint, and the
/// token it did carry stays out of the message.
#[tokio::test]
async fn an_answer_that_is_not_a_bearer_token_is_refused_without_quoting_it() {
    const TOKEN: &str = "ya29.must-not-reach-the-log";
    let cases = [
        (
            "not a bearer token",
            json!({ "access_token": TOKEN, "expires_in": 3600, "token_type": "mac" }),
        ),
        (
            "no lifetime",
            json!({ "access_token": TOKEN, "token_type": "Bearer" }),
        ),
        (
            "a lifetime serde would quote",
            json!({ "access_token": TOKEN, "expires_in": TOKEN, "token_type": "Bearer" }),
        ),
    ];
    for (case, body) in cases {
        let server = token_endpoint(ResponseTemplate::new(200).set_body_json(body), 1).await;
        let err = cache(&server)
            .token(AccountId::new(), &key_json().to_string(), None)
            .await
            .expect_err(case);
        assert!(
            matches!(err, Error::UpstreamUnavailable { lasting: false, .. }),
            "{case}: {err:?}"
        );
        assert_quotes_nothing(&err.to_string(), &[TOKEN]);
        server.verify().await;
    }
}

/// The key's own `token_uri` is where whoever wrote the file wants the grant
/// to go. It goes to the configured endpoint, addressed to Google's.
#[tokio::test]
async fn the_keys_own_token_uri_is_never_used() {
    let theirs = token_endpoint(granted("ya29.theirs"), 0).await;
    let ours = token_endpoint(granted("ya29.ours"), 1).await;
    let mut key = key_json();
    key["token_uri"] = json!(token_url(&theirs));

    let token = cache(&ours)
        .token(AccountId::new(), &key.to_string(), None)
        .await
        .expect("a token");
    assert_eq!(token, "ya29.ours");
    assert_eq!(
        claims(&grants(&ours).await[0])["aud"],
        "https://oauth2.googleapis.com/token"
    );
    theirs.verify().await;
    ours.verify().await;
}

#[tokio::test]
async fn debug_output_shows_neither_the_key_nor_a_token() {
    let server = token_endpoint(granted("ya29.held-in-the-cache"), 1).await;
    let cache = cache(&server);
    cache
        .token(AccountId::new(), &key_json().to_string(), None)
        .await
        .expect("a token");
    let shown = format!("{cache:?}");
    assert!(shown.contains(&token_url(&server)), "{shown}");
    assert_quotes_nothing(&shown, &["ya29.held-in-the-cache"]);

    let key = ServiceAccountKey::from_json(&key_json().to_string()).expect("a key");
    let shown = format!("{key:?}");
    assert!(shown.contains(EMAIL) && shown.contains(KEY_ID), "{shown}");
    assert_quotes_nothing(&shown, &[]);
}

/// Without an injected clock the assertion is dated by the system's.
#[tokio::test]
async fn the_default_clock_is_the_system_clock() {
    let server = token_endpoint(granted("ya29.now"), 1).await;
    let before = time::OffsetDateTime::now_utc().unix_timestamp();
    GcpTokenCache::new(token_url(&server))
        .expect("a client")
        .token(AccountId::new(), &key_json().to_string(), None)
        .await
        .expect("a token");
    let after = time::OffsetDateTime::now_utc().unix_timestamp();

    let iat = claims(&grants(&server).await[0])["iat"]
        .as_i64()
        .expect("iat");
    assert!(
        (before - 30..=after - 30).contains(&iat),
        "{before} - 30 <= {iat} <= {after} - 30"
    );
}

/// A credential's proxy carries every call made with it, and a mint is one.
/// The token URL here is a closed port, so a grant that reaches a token
/// endpoint at all went through the proxy, addressed to that URL.
#[tokio::test]
async fn a_mint_goes_through_the_accounts_proxy() {
    const CLOSED: &str = "http://127.0.0.1:1/token";
    let proxy = token_endpoint(granted("ya29.proxied"), 1).await;
    let cache = GcpTokenCache::new(CLOSED)
        .expect("a URL")
        .with_clock(fixed_clock);
    let key = key_json().to_string();

    let direct = cache
        .token(AccountId::new(), &key, None)
        .await
        .expect_err("nothing listens there");
    assert!(
        matches!(direct, Error::UpstreamUnavailable { lasting: false, .. }),
        "{direct:?}"
    );

    let token = cache
        .token(AccountId::new(), &key, Some(&proxy.uri()))
        .await
        .expect("through the proxy");
    assert_eq!(token, "ya29.proxied");
    let received = proxy.received_requests().await.expect("recording is on");
    assert_eq!(
        received[0].url.as_str(),
        CLOSED,
        "the proxy is asked for the token URL itself"
    );
    assert_eq!(
        claims(&grants(&proxy).await[0])["aud"],
        "https://oauth2.googleapis.com/token"
    );
    proxy.verify().await;
}

/// A token URL that is not one is a config error when the cache is made,
/// which is when the gateway starts, not on the first Vertex request.
#[test]
fn a_token_url_that_is_not_http_is_refused_when_the_cache_is_made() {
    for bad in [
        "",
        "oauth2.googleapis.com/token",
        "ftp://example.test/token",
        "file:///etc/passwd",
    ] {
        let err = GcpTokenCache::new(bad).expect_err(bad);
        assert!(matches!(err, Error::Config(_)), "{bad:?}: {err:?}");
        assert!(err.to_string().contains("Google token URL"), "{err}");
    }
    GcpTokenCache::new(DEFAULT_TOKEN_URL).expect("Google's own");
    GcpTokenCache::new("http://127.0.0.1:9/token").expect("a stand-in's");
}

/// What `account add` checks before it seals a key: the mint's own reading,
/// naming the account the key is for, and refusing what could never mint
/// without quoting any of it.
#[test]
fn a_key_is_checked_as_the_mint_reads_it_and_names_its_account() {
    assert_eq!(
        check_key(&key_json().to_string()).expect("the test key"),
        EMAIL
    );

    let with = |field: &str, value: Option<Value>| {
        let mut key = key_json();
        let object = key.as_object_mut().expect("an object");
        match value {
            Some(value) => object.insert(field.to_owned(), value),
            None => object.remove(field),
        };
        key.to_string()
    };
    let cases = [
        ("another type", with("type", Some(json!("authorized_user")))),
        ("no client_email", with("client_email", None)),
        (
            "an empty client_email",
            with("client_email", Some(json!(" "))),
        ),
        ("no private_key", with("private_key", None)),
        (
            "a PKCS#1 key",
            with(
                "private_key",
                Some(json!(TEST_KEY.replace("PRIVATE KEY", "RSA PRIVATE KEY"))),
            ),
        ),
        ("an API key", "AIzaSy-not-a-service-account".to_owned()),
    ];
    for (case, sa_json) in cases {
        let err = check_key(&sa_json).expect_err(case);
        assert!(matches!(err, Error::Config(_)), "{case}: {err:?}");
        assert_quotes_nothing(&err.to_string(), &["AIzaSy-not-a-service-account"]);
    }
}

/// Google's documentation fixes an access token request's audience: "When
/// making an access token request this value is always
/// `https://oauth2.googleapis.com/token`". Wherever the grant is posted (a
/// stand-in here, an egress proxy or a private name for the endpoint in a
/// deployment), the assertion is addressed to Google's token endpoint, or
/// Google refuses it.
#[tokio::test]
async fn the_assertion_is_addressed_to_google_wherever_it_is_posted() {
    let server = token_endpoint(granted("ya29.addressed"), 1).await;
    cache(&server)
        .token(AccountId::new(), &key_json().to_string(), None)
        .await
        .expect("a token");
    assert_eq!(
        claims(&grants(&server).await[0])["aud"],
        "https://oauth2.googleapis.com/token"
    );
    server.verify().await;
}

/// A signed assertion is as good as the key for the hour it is valid, so it
/// crosses no network in the clear: the token URL is https, or http only to
/// this machine's own loopback, where a stand-in or a local proxy listens.
#[test]
fn a_token_url_off_loopback_must_be_https() {
    for bad in [
        "http://oauth2.googleapis.com/token",
        "http://token-proxy.internal/token",
        "http://10.0.0.7/token",
    ] {
        let err = GcpTokenCache::new(bad).expect_err(bad);
        assert!(matches!(err, Error::Config(_)), "{bad:?}: {err:?}");
        assert!(err.to_string().contains("Google token URL"), "{err}");
    }
    for good in [
        DEFAULT_TOKEN_URL,
        "https://token-proxy.internal/token",
        "http://127.0.0.1:9/token",
        "http://[::1]:9/token",
        "http://localhost:9/token",
    ] {
        GcpTokenCache::new(good).expect(good);
    }
}

/// A host whose clock runs a little ahead of Google's would sign an `iat`
/// Google reads as the future, and be refused `invalid_grant`. The assertion
/// is dated thirty seconds back, and lives the full hour from then.
#[tokio::test]
async fn an_assertion_is_dated_thirty_seconds_back_for_an_hour() {
    let server = token_endpoint(granted("ya29.dated"), 1).await;
    cache(&server)
        .token(AccountId::new(), &key_json().to_string(), None)
        .await
        .expect("a token");
    let claims = claims(&grants(&server).await[0]);
    assert_eq!(claims["iat"], T0 - 30);
    assert_eq!(claims["exp"], T0 - 30 + 3600);
    server.verify().await;
}

/// What a token endpoint writes in `error_description` reaches the error
/// only when it can do no harm there: a known code's, within RFC 6749's
/// characters and a line long, and holding no part of the assertion just
/// sent. An endpoint that echoes the grant back has it dropped.
#[tokio::test]
async fn a_description_that_echoes_the_assertion_or_breaks_the_rfc_is_dropped() {
    type Describe = fn(&str) -> String;
    let cases: [(&str, Describe); 4] = [
        ("the whole assertion", |assertion| assertion.to_owned()),
        ("a piece of it", |assertion| {
            format!("Invalid JWT {}", &assertion[assertion.len() - 40..])
        }),
        ("a quote", |_| "the \"key\" is bad".to_owned()),
        ("a page of it", |_| "x".repeat(201)),
    ];
    for (case, describe) in cases {
        let echo = move |request: &Request| {
            let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
                .into_owned()
                .collect();
            ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": describe(&form["assertion"]),
            }))
        };
        let server = token_endpoint(echo, 1).await;
        let err = cache(&server)
            .token(AccountId::new(), &key_json().to_string(), None)
            .await
            .expect_err(case);
        let message = err.to_string();
        assert!(message.ends_with("(invalid_grant)"), "{case}: {message}");
        let sent = grants(&server).await;
        assert_quotes_nothing(&message, &[sent[0]["assertion"].as_str()]);
        server.verify().await;
    }
}

/// Google's refusal of a key it no longer honours.
fn refused_grant() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "error": "invalid_grant",
        "error_description": "Invalid JWT Signature.",
    }))
}

/// Ten callers arriving at once for one account, while the one mint in
/// flight is refused: every caller is told the refusal, and Google is asked
/// once. Without the memo each waiter made an attempt of its own in turn,
/// each up to the mint timeout, so ten callers were ten grants.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiters_behind_a_failed_mint_share_its_failure() {
    const CALLERS: usize = 10;
    // Slow enough that every caller arrives while the first mint is in flight.
    let server = token_endpoint(refused_grant().set_delay(Duration::from_millis(300)), 1).await;
    let cache = Arc::new(cache(&server));
    let (account, key) = (AccountId::new(), key_json().to_string());
    let start = Arc::new(tokio::sync::Barrier::new(CALLERS));

    let callers: Vec<_> = (0..CALLERS)
        .map(|_| {
            let (cache, key, start) = (Arc::clone(&cache), key.clone(), Arc::clone(&start));
            tokio::spawn(async move {
                start.wait().await;
                cache.token(account, &key, None).await
            })
        })
        .collect();
    for caller in callers {
        let err = caller
            .await
            .expect("the caller ran")
            .expect_err("the mint was refused");
        assert!(err.to_string().contains("invalid_grant"), "{err}");
        assert!(
            matches!(err, Error::UpstreamUnavailable { lasting: true, .. }),
            "{err:?}"
        );
    }
    server.verify().await;
}

/// A refused mint is remembered for fifteen seconds: a caller inside them is
/// told the refusal without asking Google, and the first one after them asks
/// again.
#[tokio::test]
async fn a_failed_mint_is_remembered_for_fifteen_seconds() {
    static NOW: AtomicI64 = AtomicI64::new(T0);
    fn now() -> i64 {
        NOW.load(Ordering::SeqCst)
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(refused_grant())
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(granted("ya29.after"))
        .expect(1)
        .mount(&server)
        .await;
    let cache = GcpTokenCache::new(token_url(&server))
        .expect("a client")
        .with_clock(now);
    let (account, key) = (AccountId::new(), key_json().to_string());

    let first = cache.token(account, &key, None).await.expect_err("refused");
    NOW.store(T0 + 14, Ordering::SeqCst);
    let remembered = cache
        .token(account, &key, None)
        .await
        .expect_err("still refused, without asking");
    assert_eq!(remembered.to_string(), first.to_string());
    assert_eq!(grants(&server).await.len(), 1, "Google was asked once");

    NOW.store(T0 + 15, Ordering::SeqCst);
    assert_eq!(
        cache.token(account, &key, None).await.expect("asked again"),
        "ya29.after"
    );
    server.verify().await;
}

/// A token past the point it is replaced is still a token. When the mint
/// that should replace it fails, it is handed out while it has more than
/// thirty seconds left; inside those, the failure is the caller's.
#[tokio::test]
async fn a_token_outlives_a_failed_refresh_until_its_last_thirty_seconds() {
    static NOW: AtomicI64 = AtomicI64::new(T0);
    fn now() -> i64 {
        NOW.load(Ordering::SeqCst)
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(granted("ya29.first"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    // Google, briefly unreachable from here on.
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let cache = GcpTokenCache::new(token_url(&server))
        .expect("a client")
        .with_clock(now);
    let (account, key) = (AccountId::new(), key_json().to_string());

    assert_eq!(
        cache.token(account, &key, None).await.expect("minted"),
        "ya29.first"
    );
    // Two hundred seconds left: due to be replaced, and the replacement fails.
    NOW.store(T0 + 3400, Ordering::SeqCst);
    assert_eq!(
        cache
            .token(account, &key, None)
            .await
            .expect("the token in hand"),
        "ya29.first"
    );
    assert_eq!(grants(&server).await.len(), 2, "the refresh was tried");
    // Thirty-one seconds left, and the refresh still fails.
    NOW.store(T0 + 3569, Ordering::SeqCst);
    assert_eq!(
        cache
            .token(account, &key, None)
            .await
            .expect("still the token in hand"),
        "ya29.first"
    );
    // Twenty-nine left: too few to set out with.
    NOW.store(T0 + 3571, Ordering::SeqCst);
    let err = cache
        .token(account, &key, None)
        .await
        .expect_err("too close to its expiry");
    assert!(err.to_string().contains("503"), "{err}");
    server.verify().await;
}

/// A token the upstream refused is dropped, so the next caller mints a new
/// one; but only if it is still the one refused, so a token another request
/// minted in the meantime is kept.
#[tokio::test]
async fn forgetting_a_token_keeps_a_newer_one() {
    static NOW: AtomicI64 = AtomicI64::new(T0);
    fn now() -> i64 {
        NOW.load(Ordering::SeqCst)
    }
    let server = MockServer::start().await;
    for (token, once) in [("ya29.refused", true), ("ya29.fresh", false)] {
        let mock = Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(granted(token))
            .expect(1);
        let mock = if once { mock.up_to_n_times(1) } else { mock };
        mock.mount(&server).await;
    }
    let cache = GcpTokenCache::new(token_url(&server))
        .expect("a client")
        .with_clock(now);
    let (account, key) = (AccountId::new(), key_json().to_string());

    assert_eq!(
        cache.token(account, &key, None).await.expect("minted"),
        "ya29.refused"
    );
    cache.forget(account, "ya29.refused").await;
    assert_eq!(
        cache
            .token(account, &key, None)
            .await
            .expect("minted again"),
        "ya29.fresh"
    );
    // A late word about the old token leaves the new one where it is.
    cache.forget(account, "ya29.refused").await;
    assert_eq!(
        cache.token(account, &key, None).await.expect("kept"),
        "ya29.fresh"
    );
    server.verify().await;
}
