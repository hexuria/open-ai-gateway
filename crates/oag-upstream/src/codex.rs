//! The Codex adapter: a `ChatGPT`/Codex subscription as an inference upstream.
//!
//! Codex speaks the Responses API, not Chat Completions — so this reuses the
//! Responses codec wholesale (`oag_proto::responses`) and adds only what the
//! subscription backend needs on top of it: its own base URL
//! (`chatgpt.com/backend-api/codex/responses`), the account-scoped auth
//! headers, and `store:false` with encrypted-reasoning replay.
//!
//! ## The instructions are yours to supply
//!
//! The backend validates the request's `instructions` against what the official
//! Codex client sends. This adapter does not compile that string in: configure
//! `gateway.codex.instructions` (or `instructions_path`; `deploy/codex-instructions.txt`
//! is a current copy, taken from the installed Codex/opencodex catalog) or the
//! client's own system prompt is passed through, which the backend will reject.
//! Keep the file in lockstep with the Codex client version.
//!
//! ## Compliance
//!
//! A subscription seat is sanctioned for its own holder's use (`docs/compliance.md`).
//! This adapter presents the Codex client's identifying headers because the
//! backend requires them; using it against an account that is not yours, or
//! pooling one account across many users, is the case OpenAI's terms forbid.
//! Bind each seat to its owner (`add-account --owner-email`).

use crate::adapter::{ProviderAdapter, UpstreamRequest};
use async_trait::async_trait;
use oag_core::credential::SecretMaterial;
use oag_core::provider::Dialect;
use oag_core::{Error, Provider, Result};
use oag_proto::{StreamAccumulator, StreamEvent, responses};
use serde_json::json;

/// The public `ChatGPT`/Codex subscription backend. The `/responses` path is
/// appended to it.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// How the Codex client names itself. A protocol identifier, not a secret; the
/// backend expects it and rejects a request without it.
pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";

/// Talks the Responses dialect to a `ChatGPT`/Codex subscription.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    base_url: String,
    /// Where the OAuth pair is refreshed; only the refresh path reads it.
    token_url: String,
    /// The `instructions` the backend expects. `None` passes the client's own
    /// system prompt through — see the module docs on why OAG ships no prompt.
    instructions: Option<String>,
    /// `OpenAI-Beta` header value, if the backend needs one.
    beta: Option<String>,
    /// `originator` header — the client's self-identification.
    originator: String,
    /// `User-Agent` header.
    user_agent: String,
}

impl CodexAdapter {
    /// A Codex adapter with the public defaults and no instructions (pass-through).
    #[must_use]
    pub fn new() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            token_url: crate::openai_oauth::DEFAULT_TOKEN_URL.to_owned(),
            instructions: None,
            beta: None,
            originator: DEFAULT_ORIGINATOR.to_owned(),
            user_agent: format!("{DEFAULT_ORIGINATOR}/unknown"),
        }
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the token endpoint, for tests against a local mock.
    #[must_use]
    pub fn with_token_url(mut self, token_url: impl Into<String>) -> Self {
        self.token_url = token_url.into();
        self
    }

    /// Set the `instructions` the backend validates against. Empty or blank is
    /// treated as "pass the client's own system prompt through".
    #[must_use]
    pub fn with_instructions(mut self, instructions: Option<String>) -> Self {
        self.instructions = instructions.filter(|s| !s.trim().is_empty());
        self
    }

    #[must_use]
    pub fn with_beta(mut self, beta: Option<String>) -> Self {
        self.beta = beta.filter(|s| !s.trim().is_empty());
        self
    }

    #[must_use]
    pub fn with_originator(mut self, originator: impl Into<String>) -> Self {
        self.originator = originator.into();
        self
    }

    #[must_use]
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for CodexAdapter {
    fn provider(&self) -> Provider {
        // A Codex seat is an OpenAI credential; only the dialect differs.
        Provider::OpenAI
    }

    fn dialect(&self) -> Dialect {
        // The whole reason this override exists. `provider()` says OpenAI and
        // `Provider::OpenAI::native_dialect()` says Chat Completions, which is
        // true of an API-key seat and false of this one. A caller that asks the
        // provider concludes the dialects already agree and forwards these
        // bytes verbatim — and a Chat Completions client reads a 200 with
        // nothing it recognises in it.
        Dialect::OpenAIResponses
    }

    fn always_streams(&self) -> bool {
        // See `build`: `stream` is forced on below, whatever the client sent.
        // Declared here so the response path reads the stream it will get
        // rather than the JSON body the client asked for.
        true
    }

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        // Codex is the Responses dialect; reuse the codec and add only what the
        // subscription backend needs on top.
        let mut body = responses::render_request(req.canonical, &req.model.upstream_name)?;
        // A streaming SSE backend that never persists a turn server-side; with
        // store:false it wants the encrypted reasoning replayed back to it.
        // Forced regardless of `req.canonical.stream` — which is exactly why
        // `always_streams` above says so.
        body["stream"] = json!(true);
        body["store"] = json!(false);
        body["include"] = json!(["reasoning.encrypted_content"]);
        // ChatGPT-account Codex rejects `max_output_tokens` (HTTP 400
        // "Unsupported parameter"); the official client does not send it.
        if let Some(obj) = body.as_object_mut() {
            obj.remove("max_output_tokens");
        }
        if let Some(instructions) = &self.instructions {
            body["instructions"] = json!(instructions);
        }

        let cred: &SecretMaterial = req.credential;
        let mut builder = reqwest::Client::new()
            .post(format!("{}/responses", self.base_url))
            .header("accept", "text/event-stream")
            .header("authorization", format!("Bearer {}", cred.access_token))
            .header("originator", self.originator.as_str())
            .header("user-agent", self.user_agent.as_str())
            // A fresh id per request: the backend wants one present, and a
            // stateless gateway has no conversation to key it to.
            .header("session_id", uuid::Uuid::new_v4().to_string());

        // Account-scoped: the header binds the request to the seat the token
        // belongs to. A seat imported by `--from codex` always carries it.
        if let Some(account_id) = &cred.account_id {
            builder = builder.header("chatgpt-account-id", account_id.as_str());
        }
        if let Some(beta) = &self.beta {
            builder = builder.header("openai-beta", beta.as_str());
        }

        builder
            .json(&body)
            .build()
            .map_err(|e| Error::Internal(format!("building codex request: {e}")))
    }

    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>> {
        responses::parse_event(raw, acc)
    }

    async fn refresh(&self, credential: &SecretMaterial) -> Result<Option<SecretMaterial>> {
        // The same OAuth refresh as an imported Codex seat: trade the pair at
        // the token endpoint. No-ops on a credential with no refresh token.
        crate::openai_oauth::refresh(credential, &self.token_url).await
    }

    async fn served_models(&self, credential: &SecretMaterial) -> Result<Option<Vec<String>>> {
        // Same auth and same account scoping as `build`: the answer is
        // per-seat, so asking without `chatgpt-account-id` would be asking a
        // different question from the one inference will ask.
        let mut builder = reqwest::Client::new()
            .get(format!("{}/models", self.base_url))
            // Required, and the backend says so rather than guessing for us:
            // without it the answer is a 400 naming `('query', 'client_version')`
            // as a missing field. Derived from `user_agent` rather than given a
            // config knob of its own, so the two cannot disagree about which
            // client this is claiming to be.
            .query(&[("client_version", client_version(&self.user_agent))])
            .header("accept", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", credential.access_token),
            )
            .header("originator", self.originator.as_str())
            .header("user-agent", self.user_agent.as_str());
        if let Some(account_id) = &credential.account_id {
            builder = builder.header("chatgpt-account-id", account_id.as_str());
        }

        let response = builder
            .send()
            .await
            .map_err(|e| Error::Internal(format!("asking codex for its models: {e}")))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Internal(format!(
                "codex models returned {status}: {}",
                truncate(&body)
            )));
        }

        parse_served(&body).map(Some)
    }
}

/// Pull model ids out of whatever shape the backend answers with.
///
/// Split out and pure because the shape is the part we are least sure of: this
/// endpoint is undocumented, and a parser that can be tested against a captured
/// body is worth more than one that can only be exercised against a live seat.
/// Accepts the OpenAI listing shape (`{"data":[{"id":...}]}`) and a bare array
/// of strings, and fails loudly with the body rather than returning an empty
/// list — "served nothing" is a claim callers act on, so it must never be what
/// an unrecognised payload degrades into.
fn parse_served(body: &str) -> Result<Vec<String>> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| Error::Internal(format!("codex models is not JSON: {e}")))?;

    // `models`/`slug` is what the Codex backend actually answers; `data`/`id`
    // is the OpenAI listing shape. Both are accepted because this adapter is
    // pointed at either in different deployments, and neither costs anything to
    // keep.
    let rows = match &json {
        serde_json::Value::Object(o) => o
            .get("models")
            .or_else(|| o.get("data"))
            .and_then(|d| d.as_array()),
        serde_json::Value::Array(rows) => Some(rows),
        _ => None,
    };
    let ids: Vec<String> = rows
        .map(|rows| {
            rows.iter()
                .filter_map(|r| match r {
                    serde_json::Value::String(s) => Some(s.clone()),
                    other => other["slug"]
                        .as_str()
                        .or_else(|| other["id"].as_str())
                        .map(str::to_owned),
                })
                .collect()
        })
        .unwrap_or_default();

    if ids.is_empty() {
        return Err(Error::Internal(format!(
            "codex models: no ids in a payload shaped {}",
            truncate(body)
        )));
    }
    Ok(ids)
}

/// What to send as `client_version` when the user agent carries no version.
///
/// The backend validates the SHAPE — `codex_cli_rs/unknown`, which is the
/// shipped default, is refused with "Invalid `client_version` format" — so the
/// field cannot be filled with a placeholder. This is the Codex CLI release
/// this adapter is written against; see the lockstep note at the top of the
/// file. An operator running a different client sets `gateway.codex.user_agent`
/// to `name/version` and that version is used instead.
const DEFAULT_CLIENT_VERSION: &str = "0.152.1";

/// The version half of a `name/version` user agent, or a usable default.
///
/// `codex_cli_rs/0.104.0` yields `0.104.0`. Anything not shaped like a version
/// — the default `unknown`, or a user agent with no slash at all — falls back
/// to [`DEFAULT_CLIENT_VERSION`], because the backend rejects a malformed value
/// as firmly as a missing one and a refused discovery leaves the served set
/// NULL for that credential.
fn client_version(user_agent: &str) -> &str {
    let candidate = user_agent.rsplit_once('/').map_or(user_agent, |(_, v)| v);
    if candidate.starts_with(|c: char| c.is_ascii_digit()) {
        candidate
    } else {
        DEFAULT_CLIENT_VERSION
    }
}

/// Keep an upstream body short enough to belong in an error message.
fn truncate(body: &str) -> String {
    const MAX: usize = 300;
    if body.len() <= MAX {
        return body.to_owned();
    }
    let cut = body
        .char_indices()
        .take_while(|(i, _)| *i <= MAX)
        .last()
        .map_or(0, |(i, _)| i);
    format!("{}…", &body[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_proto::{CanonicalRequest, ContentBlock, Message, Role};
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::dec;
    use serde_json::Value;

    #[test]
    fn a_client_version_is_the_half_after_the_slash() {
        assert_eq!(super::client_version("codex_cli_rs/0.104.0"), "0.104.0");
        // Only the LAST slash separates, so a versioned path does not lose its
        // tail to an earlier one.
        assert_eq!(super::client_version("a/b/1.2.3"), "1.2.3");
        // The shipped default carries no version, and the backend refuses a
        // non-version with "Invalid client_version format" — so a placeholder
        // must not be passed through as though it were one.
        assert_eq!(
            super::client_version("codex_cli_rs/unknown"),
            super::DEFAULT_CLIENT_VERSION
        );
        assert_eq!(super::client_version("bare"), super::DEFAULT_CLIENT_VERSION);
    }

    #[test]
    fn the_openai_listing_shape_parses() {
        let body = r#"{"object":"list","data":[
            {"id":"gpt-5.6-luna","object":"model"},
            {"id":"gpt-5.6-terra","object":"model"}]}"#;
        assert_eq!(
            super::parse_served(body).expect("parsed"),
            ["gpt-5.6-luna", "gpt-5.6-terra"]
        );
    }

    #[test]
    fn the_codex_backend_shape_parses() {
        // What the live backend actually answers: `models`, keyed by `slug`,
        // with a pile of capability fields we do not read. Captured from a real
        // response rather than imagined.
        let body = r#"{"models":[
            {"slug":"gpt-reserve","prefer_websockets":true,"default_verbosity":"low"},
            {"slug":"gpt-5.6-luna","input_modalities":["text","image"]}]}"#;
        assert_eq!(
            super::parse_served(body).expect("parsed"),
            ["gpt-reserve", "gpt-5.6-luna"]
        );
    }

    #[test]
    fn a_bare_array_of_names_parses_too() {
        // The endpoint is undocumented, so the shape is the part we are least
        // sure of. Accepting both costs one match arm and saves a release.
        assert_eq!(
            super::parse_served(r#"["gpt-5.4-mini","gpt-5.5"]"#).expect("parsed"),
            ["gpt-5.4-mini", "gpt-5.5"]
        );
    }

    #[test]
    fn an_unrecognised_payload_fails_rather_than_reading_as_empty() {
        // The whole reason this is a Result. An empty Vec is the claim "this
        // credential serves nothing", which hides every one of its models from
        // the picker -- so a shape we did not anticipate must never decay into
        // it. The body rides along in the error because the next person to see
        // this needs to know what the backend actually said.
        let err = super::parse_served(r#"{"models":{"unexpected":"shape"}}"#)
            .expect_err("an unknown shape must not read as 'serves nothing'");
        let text = err.to_string();
        assert!(text.contains("unexpected"), "the body is missing: {text}");
    }

    #[test]
    fn a_non_json_body_is_an_error_not_an_empty_list() {
        assert!(super::parse_served("<html>502 Bad Gateway</html>").is_err());
    }

    #[test]
    fn a_long_body_is_truncated_on_a_character_boundary() {
        // Multi-byte input, because slicing a String by byte offset is how a
        // diagnostic path panics on the one request that needed diagnosing.
        let long = "\u{e9}".repeat(500);
        let out = super::truncate(&long);
        assert!(out.chars().count() < 500, "not truncated");
        assert!(out.ends_with('\u{2026}'));
    }

    fn model() -> ModelSpec {
        ModelSpec {
            id: ModelId::new("openai/gpt-5-codex"),
            provider: Provider::OpenAI,
            upstream_name: "gpt-5-codex".to_owned(),
            pricing: Pricing {
                input_per_mtok: dec!(1),
                output_per_mtok: dec!(2),
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 256_000,
            max_output_tokens: 16_384,
            capabilities: Capabilities::default(),
            display_label: None,
        }
    }

    fn request() -> CanonicalRequest {
        CanonicalRequest {
            model: "oag/auto".to_owned(),
            system: vec![ContentBlock::Text {
                text: "client system prompt".to_owned(),
                cache_control: None,
            }],
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_owned(),
                    cache_control: None,
                }],
            }],
            tools: vec![],
            max_tokens: 256,
            stream: false,
            temperature: None,
            thinking_budget: None,
            thinking_effort: None,
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
        }
    }

    fn seat() -> SecretMaterial {
        SecretMaterial {
            access_token: "seat-token".to_owned(),
            refresh_token: Some("r".to_owned()),
            expires_at: None,
            version: 0,
            client_id: Some("codex-client".to_owned()),
            account_id: Some("acct-123".to_owned()),
        }
    }

    fn build_with(adapter: &CodexAdapter, cred: &SecretMaterial) -> reqwest::Request {
        let c = request();
        let m = model();
        adapter
            .build(&UpstreamRequest {
                canonical: &c,
                model: &m,
                credential: cred,
            })
            .expect("builds")
    }

    fn body_of(req: &reqwest::Request) -> Value {
        let bytes = req.body().and_then(reqwest::Body::as_bytes).expect("body");
        serde_json::from_slice(bytes).expect("json")
    }

    #[test]
    fn hits_the_codex_responses_endpoint() {
        let req = build_with(&CodexAdapter::new(), &seat());
        assert_eq!(
            req.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn carries_the_codex_identifying_headers() {
        let req = build_with(&CodexAdapter::new(), &seat());
        let h = req.headers();
        assert_eq!(h["authorization"], "Bearer seat-token");
        assert_eq!(h["originator"], "codex_cli_rs");
        // The account-scoped header, from the seat's account_id.
        assert_eq!(h["chatgpt-account-id"], "acct-123");
        assert!(
            h.contains_key("session_id"),
            "a session id is always present"
        );
        assert_eq!(h["accept"], "text/event-stream");
    }

    #[test]
    fn a_seat_without_an_account_id_omits_the_account_header() {
        // A plain OpenAI OAuth token with no account still builds a valid
        // request; the header is simply absent rather than empty.
        let mut cred = seat();
        cred.account_id = None;
        let req = build_with(&CodexAdapter::new(), &cred);
        assert!(!req.headers().contains_key("chatgpt-account-id"));
    }

    #[test]
    fn streams_without_server_side_storage_and_replays_reasoning() {
        // store:false is what a stateless gateway needs, and it obliges us to
        // ask for the encrypted reasoning back so a follow-up turn can replay it.
        let body = body_of(&build_with(&CodexAdapter::new(), &seat()));
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["model"], "gpt-5-codex");
        assert!(
            body.get("max_output_tokens").is_none(),
            "ChatGPT-account Codex rejects this parameter"
        );
    }

    #[test]
    fn without_configured_instructions_the_clients_prompt_passes_through() {
        // OAG embeds no Codex prompt: what the client sent as its system prompt
        // is what goes on the wire.
        let body = body_of(&build_with(&CodexAdapter::new(), &seat()));
        assert_eq!(body["instructions"], "client system prompt");
    }

    #[test]
    fn configured_instructions_override_the_clients_prompt() {
        // When the operator supplies the prompt their subscription expects, it
        // replaces whatever the client sent.
        let adapter = CodexAdapter::new().with_instructions(Some("You are Codex.".to_owned()));
        let body = body_of(&build_with(&adapter, &seat()));
        assert_eq!(body["instructions"], "You are Codex.");
    }

    #[test]
    fn blank_configured_instructions_are_treated_as_pass_through() {
        // An empty string in config must not blank out the client's prompt.
        let adapter = CodexAdapter::new().with_instructions(Some("   ".to_owned()));
        let body = body_of(&build_with(&adapter, &seat()));
        assert_eq!(body["instructions"], "client system prompt");
    }

    #[test]
    fn each_request_gets_a_fresh_session_id() {
        let a = build_with(&CodexAdapter::new(), &seat());
        let b = build_with(&CodexAdapter::new(), &seat());
        assert_ne!(a.headers()["session_id"], b.headers()["session_id"]);
    }

    #[test]
    fn originator_and_user_agent_are_overridable() {
        let adapter = CodexAdapter::new()
            .with_originator("custom_cli")
            .with_user_agent("custom_cli/1.2.3");
        let req = build_with(&adapter, &seat());
        assert_eq!(req.headers()["originator"], "custom_cli");
        assert_eq!(req.headers()["user-agent"], "custom_cli/1.2.3");
    }

    #[test]
    fn an_optional_beta_header_is_sent_only_when_configured() {
        let plain = build_with(&CodexAdapter::new(), &seat());
        assert!(!plain.headers().contains_key("openai-beta"));

        let with_beta = CodexAdapter::new().with_beta(Some("responses=experimental".to_owned()));
        let req = build_with(&with_beta, &seat());
        assert_eq!(req.headers()["openai-beta"], "responses=experimental");
    }

    #[test]
    fn parses_a_responses_text_delta() {
        // The parse path is the Responses codec verbatim; a smoke test that the
        // wiring is intact.
        let mut acc = StreamAccumulator::new();
        let events = CodexAdapter::new()
            .parse_event(
                r#"{"type":"response.output_text.delta","delta":"hello"}"#,
                &mut acc,
            )
            .expect("parses");
        assert!(matches!(
            events.first(),
            Some(StreamEvent::TextDelta { text }) if text == "hello"
        ));
    }
}
