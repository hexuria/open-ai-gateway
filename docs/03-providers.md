# Adding a provider

Two things: a `ProviderAdapter`, and catalog entries for its models.

## The adapter contract

```rust
#[async_trait]
pub trait ProviderAdapter: Send + Sync + Debug {
    fn provider(&self) -> Provider;
    fn build(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Request>;
    fn parse_event(&self, raw: &str, acc: &mut StreamAccumulator) -> Result<Vec<StreamEvent>>;
    async fn refresh(&self, cred: &SecretMaterial) -> Result<Option<SecretMaterial>>;
}
```

Deliberately narrow. Everything an adapter does *not* need to know — which
credential to use, whether to retry, what it cost — is decided before it is
called. sub2api's equivalent is duck-typed across four concrete services with no
interface at all, which is why adding a provider there means reading all four to
work out the shape.

`parse_event` returns a `Vec` because the mapping is not one-to-one: an
Anthropic `content_block_start` plus its deltas is a single OpenAI chunk, and
one OpenAI chunk carrying both content and a tool call is two canonical events.
Returning an empty vec is normal — most dialects emit bookkeeping lines that
carry nothing.

`refresh` defaults to "nothing to do", which is correct for every static API key,
so only OAuth-style adapters implement it.

## Most providers need no adapter

`Provider::native_dialect` maps a provider to the wire format it speaks. OpenAI,
Kimi, DeepSeek, Zhipu, and xAI all speak Chat Completions, so **one adapter
covers all five** — `OpenAICompatAdapter` — and they differ only in base URL and
catalog entries. Check the dialect before writing code.

Point any of them somewhere else without a rebuild:

```yaml
gateway:
  provider_base_urls:
    kimi: "https://your-proxy.internal/v1"
```

## Which dialect reaches which upstream

Any inbound dialect can reach any upstream one; translation goes through the
canonical form. When the two agree, bytes pass through **verbatim** — the
upstream's own bytes are the most faithful answer available, and re-serialising
can only differ from them.

All four dialects parse inbound and render outbound, so any client shape
reaches any upstream shape:

| Dialect | Inbound route | Renders outbound |
|---|---|---|
| Anthropic Messages | `/v1/messages` | yes |
| OpenAI Chat Completions | `/v1/chat/completions` | yes |
| OpenAI Responses | `/v1/responses` | yes |
| Gemini | `/v1beta/models/{model}:generateContent` | yes |

No provider declares Responses as its *native* dialect — the adapter registered
for OpenAI speaks Chat Completions — so reaching an upstream over Responses
would be a separate adapter, not a change to the hub.

The Anthropic direction is the harder one: it uses indexed content blocks that
must be explicitly opened and closed, so the renderer tracks the open block and
closes it before opening another. A client that receives a delta for a block it
was never told about drops it silently.

## Framing

Not every provider streams server-sent events. `ProviderAdapter::framing()`
says which one it speaks, and the default is SSE because all but one do:

| Framing | Providers |
|---|---|
| `Sse` | Anthropic, OpenAI, Gemini, Kimi, DeepSeek, Zhipu, xAI |
| `AwsEventStream` | Bedrock |

Bedrock streams length-prefixed binary messages whose payload carries the
provider's own event, base64-encoded. A reader that splits on blank lines finds
nothing in one — and the failure is silent: an empty response and zero recorded
usage, with no error anywhere. `eventstream.rs` decodes it.

This also means **a binary-framed upstream can never be passed through**, even
when the dialects match. Bedrock's dialect *is* Anthropic, so dialect alone
would say passthrough and hand an SSE client a binary envelope; `egress_for`
requires SSE framing as well.

## Transport

`Transport` is a trait with exactly one implementation: `reqwest` over rustls.

The seam exists because sub2api needs to impersonate the official CLI's TLS
fingerprint — it routes resold subscription traffic that providers actively try
to detect. This gateway does not, so the default build links no BoringSSL and
ships no impersonation code. See [compliance.md](compliance.md).

Transports are pooled per `(credential, proxy)`, not per host. Two credentials
sharing a TCP connection share whatever per-connection state the provider keeps,
so a rate limit on one takes the other down with it. The pool is bounded and
evicts by idle time; an evicted transport's in-flight requests are unaffected,
because the `Arc` outlives the cache entry — a long-running stream is never cut
short by eviction.

## Providers with their own adapter

| Provider | Why it needs one |
|---|---|
| Anthropic | The canonical dialect. |
| Gemini | Model and mode in the URL path; its own auth header; a genuinely different body shape. |
| Bedrock | Anthropic's body, but the model is in the path, `anthropic_version` replaces the version header, and every request is SigV4-signed. |

`sigv4.rs` is hand-rolled — a few dozen lines against the AWS SDK's several
hundred transitive crates and a second HTTP stack, none of which this gateway
would use for anything else. Bedrock credentials are stored packed as
`access_key:secret[:session_token]`, so Bedrock needs no separate credential
shape from every other provider.

## Catalog entries

```rust
ModelSpec {
    id: ModelId::new("kimi/k2"),          // canonical, provider/name
    provider: Provider::Kimi,
    upstream_name: "moonshot-v1-128k".into(),  // what goes on the wire
    pricing: Pricing { /* per million tokens, as Decimal */ },
    context_window: 128_000,
    max_output_tokens: 8_192,
    capabilities: Capabilities { vision: false, tools: true, .. },
}
```

Canonical ids are distinct from upstream names because Bedrock calls Sonnet
`anthropic.claude-sonnet-4-v1:0` and routing policy should not have to spell
that.

Prices are `Decimal`, never `f64`. They get multiplied by token counts and
summed across millions of rows, and there is no reason to accept binary
floating-point drift on a fixed-point quantity.

Get capabilities right. They are used to *reject* a rung before sending, so a
vision request never reaches a text-only model — a decision that is free to make
correctly and costs a round trip and a 400 to get wrong.

## Testing one

Record real request/response pairs as fixtures and assert the round trip through
the canonical hub: lossless for non-streaming, event-equivalent for streaming.
`oag-proto` is pure, so this needs no network and no database, which is what
makes a large corpus practical.
