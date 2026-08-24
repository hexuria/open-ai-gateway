# LLM mock servers we could point the gateway at

Surveyed 2026-08-24 against what this gateway actually speaks: Anthropic Messages SSE, OpenAI Chat Completions, Gemini `generateContent` / `streamGenerateContent`, Bedrock `invoke` / `invoke-with-response-stream` with AWS event-stream payload `{"bytes": "<base64 of Anthropic event>"}`, and local `/v1/messages/count_tokens` (estimate, not proxied).

Already in-tree: `deploy/test/mock-upstream.py` (Anthropic only, stdlib, ConfigMap-safe) and CopilotKit/aimock (OpenAI + Gemini + Anthropic; Bedrock encoder uses the wrong inner payload).

Every claim below that says “probed” was hit from this machine. Sources for product claims are the projects’ own repos/docs.

## Verdict

| Tool | Use here? | Why |
|---|---|---|
| **VidaiMock** | In-tree overlay for Bedrock (`just verify-bedrock`) | Bundled Bedrock is Converse + empty stream bytes. `deploy/test/vidaimock/` overlays Anthropic event JSON into `encode_chunk`. Proved through the gateway 2026-08-24. Also a strong Anthropic/OpenAI/Gemini binary if we ever drop `npx` aimock. |
| **llm-mock** (axium-lab) | Useful for `count_tokens` shape checks, not calibration | Has `POST /anthropic/v1/messages/count_tokens`. Returns an approximation (`Hello!` → 2). Requires `sk-mock-key-01` and `anthropic-version`. No Bedrock, no OAuth token URL. Dummy keys 401. |
| **aimock** (already wired) | Keep for `just verify-dialects` | OpenAI + Gemini + Anthropic on one port, fixtures, dummy keys. Bedrock payload is raw JSON, not `{"bytes":}`. |
| **Python mock** (already wired) | Keep for `just verify` / kind | Zero deps, ConfigMap. Deterministic `MOCK_FAIL_FIRST`. |
| Ollama / LocalAI / llama.cpp | Only as a real cheap OpenAI-compat upstream | Not mocks. This machine’s Ollama has **no models pulled**. No Anthropic/Bedrock/Gemini native. |
| Mokksy / AI-Mocks | Skip | Kotlin/Ktor **library** for JVM tests, not a drop-in HTTP process our Rust binary points at. |
| kagent-dev/mockllm | Weak | Go; Anthropic **non-streaming only** per its README. |
| MSW | Skip | Request interceptor inside Node. The gateway is a Rust HTTP client. |
| moto | Skip | `bedrock-runtime` still NotYetImplemented ([getmoto#7682](https://github.com/getmoto/moto/issues/7682)). |
| mockapi.dog | Skip | Hosted; `POST /v1/chat/completions` 307’d to a locale path when probed. |
| WireMock MockGPT | Skip | OpenAI-shaped; no evidence of Anthropic SSE or Bedrock event-stream. |
| llama.cpp `llama-server` | Calibration oracle only | Real GGUF tokenizer on `POST /v1/messages/count_tokens`. Not Claude’s vocab. Speaks Anthropic Messages + OpenAI. Needs a model file. |
| piyook/llm-mock, dwmkerr/mock-llm | Skip | OpenAI-shaped paths; not our adapters. |
| mokksy/ai-mocks, openai-responses, default MSW | Skip | In-process (JVM / Python httpx / Node intercept). Our client is Rust `reqwest`. |

Nothing we found mocks Anthropic OAuth token refresh. `AnthropicAdapter::refresh` is still `Ok(None)`. Nothing we found is Anthropic’s real tokenizer; llm-mock and VidaiMock Gemini `countTokens` are estimates, like ours.

## Experiments (this machine, 2026-08-24)

Containers:

```bash
docker run -d --name vidaimock -p 8100:8100 ghcr.io/vidaiuk/vidaimock:latest
docker run -d --name llm-mock  -p 3000:3000 ghcr.io/axium-lab/llm-mock:latest
```

Also hit the hosted llm-mock at `https://api.llm-mock.dev`.

### VidaiMock (`ghcr.io/vidaiuk/vidaimock:latest`)

| Probe | Result |
|---|---|
| `GET /health` | `{"status":"ok"}` |
| `POST /v1/messages` stream | HTTP 200 SSE. Events: `message_start`, `content_block_start`, **`ping`**, `content_block_delta` × N, `content_block_stop`, `message_delta`, `message_stop`. |
| `POST /v1/messages` | Anthropic body, `usage.input_tokens=16`, `output_tokens=10`. |
| `POST /v1/chat/completions` | OpenAI body, 200. |
| `POST /v1beta/models/gemini-2.5-flash:generateContent` | Gemini candidates + text. |
| `POST /v1beta/models/gemini-2.5-flash:countTokens` | `{"totalTokens": 9, "promptTokensDetails":[…]}`. |
| `POST /v1/messages/count_tokens` | **404** |
| `POST /model/…/invoke` | 200, **Converse** shape (`output.message.content[].text`), not Anthropic InvokeModel. |
| `POST /model/…/invoke-with-response-stream` | `Content-Type: application/vnd.amazon.eventstream`. 7 frames, each payload **`{"bytes":""}`**. Framing + CRC + `bytes` wrapper match `oag-upstream/eventstream.rs`. Inner event is empty, so our `parse_event` would see nothing. |
| `X-Mock-Status: 429` on `/v1/messages` | HTTP 429, Anthropic `rate_limit_error` envelope. |

Encoder source ([`src/aws_event_stream.rs`](https://github.com/vidaiUK/VidaiMock/blob/main/src/aws_event_stream.rs)): base64 the inner JSON, wrap `{"bytes": "…"}`, then AWS event-stream with CRC32. That is the Bedrock InvokeModelWithResponseStream envelope. aimock skips the wrap. VidaiMock’s **runtime** currently feeds the encoder empty strings (seven frames of `{"bytes":""}`). The bundled YAML also has a Converse-shaped `on_chunk` template; even if that filled in, the inner JSON after unwrap would not be an Anthropic `content_block_delta`.

`config/providers/bedrock.yaml` matcher is `invoke(-with-response-stream)?`; the non-stream template is Converse. Our `BedrockAdapter` builds Anthropic bodies on `/invoke` and `/invoke-with-response-stream`.

### llm-mock (`ghcr.io/axium-lab/llm-mock:latest`, v0.7.1; also `api.llm-mock.dev`)

Prefixes: `/openai/v1`, `/anthropic`, `/gemini`. Our `provider_base_urls` would be e.g. `http://127.0.0.1:3000/anthropic`.

| Probe | Result |
|---|---|
| `POST /anthropic/v1/messages` + `sk-mock-key-01` + `anthropic-version` | 200, Echo + usage. Stream has `event:` and `message_stop`. |
| Missing `anthropic-version` | 400 `anthropic-version header is required` |
| `x-api-key: FAKE-CREDENTIAL-FOR-TESTS` | **401** `invalid x-api-key` |
| `POST /anthropic/v1/messages/count_tokens` | `{"input_tokens": 2}` for `"Hello!"`. 400 `a`s → 100 (same as `len/4`). Docs: approximation, same formula as their `/messages` usage. |
| OpenAI `/openai/v1/chat/completions` | 200 |
| Gemini `/gemini/v1beta/models/…:generateContent` | 200, `usageMetadata.promptTokenCount=2` |
| `POST /anthropic/v1/oauth/token` | 401 (treated as Messages API; no token endpoint) |

Our adapter already sends `anthropic-version`. Tests would have to seal `sk-mock-key-01`, not `FAKE-CREDENTIAL-FOR-TESTS`.

Their count is **not** a calibration source. Our `count_input_tokens("Hello!")` is `len/4 = 1`; they return 2. Longer English happened to match `len/4` in the few samples.

### Ollama on this machine

`127.0.0.1:11434` is up. `GET /api/tags` → `{models:[]}`. `/v1/messages` → 404. OpenAI-compat exists only after a model is pulled. Not a mock.

### mockapi.dog

`POST https://mockapi.dog/v1/chat/completions` → HTTP 307 to `/en/v1/chat/completions`. Not a drop-in.

## How this maps to HANDOVER.md

| Gap | Closest mock | Still blocked? |
|---|---|---|
| OpenAI / Gemini adapter E2E | aimock (done), VidaiMock (probed, would work) | Cleared |
| Circuit breakers | Python `MOCK_FAIL_STATUS=408`; VidaiMock `X-Mock-Status` is 429 (rate-limit path, not RetrySameAccount) | Cleared with Python mock |
| Bedrock vs AWS framing | `just verify-bedrock` vs VidaiMock overlay (independent `{"bytes":}` + CRC32). Not AWS itself. | Real Bedrock still needed for vendor framing |
| OAuth refresh | None found | Yes |
| `count_tokens` calibration | llm-mock / VidaiMock Gemini counts are estimates | Yes — need Anthropic’s tokenizer |
| Cross-dialect (OpenAI client → Anthropic upstream) | Python mock (`just verify-translate`) | Cleared |
| Kind / ConfigMap | Still only the Python mock | Keep it |

## If we adopt one more tool

**VidaiMock as `just verify-dialects` alternative:** one binary, no `npx`, Anthropic `ping` events our Python mock never emits, Gemini `countTokens` for a future proxy path. Bedrock is already covered by `just verify-bedrock` plus `deploy/test/vidaimock/`. Do not point Bedrock at **bundled** VidaiMock (empty bytes, Converse body).

**llm-mock only if** we want a second opinion on `count_tokens` *shape* (`{input_tokens: N}`) or strict `anthropic-version` / 401 behaviour. Not for kind, not for Bedrock.

Do not add Mokksy, MSW, moto, or a hosted SaaS to CI.

## Catalogued, not probed on this machine

| Candidate | Source | Why it is not a drop-in |
|---|---|---|
| llama.cpp `llama-server` | [server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md) | OpenAI + Anthropic Messages + `count_tokens` with the **loaded GGUF tokenizer**. Only real-count oracle besides Anthropic itself. Needs weights; vocab is not Claude’s. |
| LocalAI | [localai.io](https://localai.io/docs/getting-started/models/) | OpenAI-compat, real models. |
| piyook/llm-mock (`llmock`) | [piyook/llm-mock](https://github.com/piyook/llm-mock) | Configurable OpenAI path; default is not `/v1/chat/completions`. |
| dwmkerr/mock-llm | [dwmkerr/mock-llm](https://github.com/dwmkerr/mock-llm) | OpenAI + MCP OAuth, not Anthropic. |
| kagent-dev/mockllm | [README](https://github.com/kagent-dev/mockllm) | Anthropic **non-streaming only**. |
| mokksy/ai-mocks | [README](https://github.com/mokksy/ai-mocks) | Kotlin test library; binds a port inside a JVM test, not a process we exec. |
| openai-responses | [mharrisb1/openai-responses-python](https://github.com/mharrisb1/openai-responses-python) | Patches Python httpx. Maintenance mode. |
| MSW | [mswjs.io Node](https://mswjs.io/docs/integrations/node); [`@mswjs/http-middleware`](https://github.com/mswjs/http-middleware) can bind Express | Default intercepts JS. Middleware would be DIY stubs. |
| moto bedrock-runtime | [docs](https://docs.getmoto.org/en/latest/docs/services/bedrock-runtime.html) | `invoke_model` returns `{}`. `invoke_model_with_response_stream` unimplemented. |
| ailib-official/ai-protocol-mock | [README](https://github.com/ailib-official/ai-protocol-mock) | OpenAI + Anthropic JSON for ai-lib. SSE/Gemini unverified. |
| WireMock MockGPT | [2013/2023 blog](https://www.wiremock.io/post/mockgpt-mock-openai-api) | Canned OpenAI; not a dialect pack. |

## Sources

- [VidaiMock README](https://github.com/vidaiUK/VidaiMock) · [encoder](https://github.com/vidaiUK/VidaiMock/blob/main/src/aws_event_stream.rs) · [bedrock.yaml](https://github.com/vidaiUK/VidaiMock/blob/main/config/providers/bedrock.yaml)
- [axium-lab/llm-mock README](https://github.com/axium-lab/llm-mock) · [Anthropic API page](https://llm-mock.dev/api-anthropic.html)
- [CopilotKit/aimock](https://github.com/CopilotKit/aimock)
- [kagent-dev/mockllm](https://github.com/kagent-dev/mockllm)
- [mokksy/ai-mocks](https://github.com/mokksy/ai-mocks)
- [getmoto/moto#7682](https://github.com/getmoto/moto/issues/7682)
- Probes: local Docker `vidaimock` + `llm-mock` 0.7.1, `api.llm-mock.dev`, local Ollama `:11434`, `mockapi.dog` (2026-08-24)
