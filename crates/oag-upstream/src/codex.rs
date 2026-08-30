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

    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request> {
        // Codex is the Responses dialect; reuse the codec and add only what the
        // subscription backend needs on top.
        let mut body = responses::render_request(req.canonical, &req.model.upstream_name)?;
        // A streaming SSE backend that never persists a turn server-side; with
        // store:false it wants the encrypted reasoning replayed back to it.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_proto::{CanonicalRequest, ContentBlock, Message, Role};
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    use rust_decimal::dec;
    use serde_json::Value;

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
