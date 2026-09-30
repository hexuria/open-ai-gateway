# Credential kinds and their standing

This gateway is built for **one organisation, using its own credentials, for its
own members**. It is not a resale product and has no billing, payments, or
public signup — those were deliberately left out.

That framing matters, because providers draw a sharp line between "pool your own
credentials for your own people" and "route other people's traffic through a
subscription". The first is explicitly permitted. The second is not.

The gateway supports every kind below and is indifferent to which you use. The
choice is the operator's, and the schema records it so it is visible.

## The kinds

| Kind | Standing | Use for |
|---|---|---|
| `api_key` | **Sanctioned.** Explicitly permitted for the customer's own authorised users. | The default. Pool and rotate freely. |
| `bedrock` | **Sanctioned**, governed by your cloud agreement. | Deployments already on AWS. (`account add` stores Bedrock keys as `api_key`; `bedrock` is accepted for rows written by hand.) |
| `oauth` — Team/Enterprise seat | **Sanctioned.** OAuth covers Free, Pro, Max, Team, and Enterprise purchasers. | Per-person binding: each member signs in with their own seat. |
| `oauth` — individual Pro/Max seat | **Its holder's own use only.** These plans assume ordinary, individual usage. | One person reaching their own seat. OAG refuses to share one: see below. |

## What the providers actually say

Anthropic's [Claude Code legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance)
sets out both halves. The restriction:

> Anthropic does not permit third-party developers to offer Claude.ai login into
> their own applications, or to route requests through Free, Pro, or Max plan
> credentials on behalf of their users. Moreover, developers may not collect,
> store, or intermediate Claude.ai credentials or session tokens.

And the carve-out that covers this gateway's intended use:

> This does not restrict how customers provision and manage their own API keys
> or third-party inference provider credentials — for example, configuring an
> API key in a development environment, secrets manager, or machine image for
> use by the customer's own authorized users — provided the resulting usage is
> billed to the key owner under their agreement with Anthropic (or the
> applicable provider) and is not resold or intermediated as described above.

OpenAI's terms are equivalent: they prohibit sharing account credentials and
using ChatGPT to power third-party services, and ChatGPT subscription
credentials are separate from API credentials in any case.

## The distinction that matters

Not "OAuth versus API key". It is **per-principal binding versus shared
pooling**, and it is one nullable column:

```sql
account.owner_principal_id  uuid REFERENCES principal(id)
```

- **Set** — the credential belongs to one person, and only their requests use
  it. A Team or Enterprise seat holder reaching their own seat through the
  gateway is doing ordinary individual usage; the gateway is routing and
  metering, not intermediating someone else's credential.
- **NULL** — the credential joins the shared pool, available to every request on
  its routes. Correct for `api_key` and `bedrock`, and **never** for a
  subscription seat.

**A subscription seat belongs to exactly one person.** One person may own
several seats; a seat never serves anyone but its owner. Sharing one personal
plan across people is what those plans' terms forbid, and it is what got an
account banned. So the gateway does not offer it:

- `oag admin account add --from grok|codex` requires `--owner-email`;
  `--shared` is gone and names this rule when passed.
- The schema refuses to make an owner-less seat — inserting one, clearing a
  seat's owner, or turning an owner-less key into a seat — with the
  `account_seat_has_one_owner` trigger (migration 0019).
- A seat left owner-less by an older version serves no one — the request path
  matches it for nobody — until `oag admin account set-owner <name>
  --owner-email <email>` binds it. `oag admin doctor` lists every such seat.
  Until then it can still be disabled, renamed or priced, and the usage
  poller neither reads nor refreshes it.
- A seat's owner is a principal, and a principal can hold several keys. That
  is fine when they are all that person's (a laptop, a CI job); a key handed to
  someone else shares the seat. The gateway cannot tell the two apart, so
  `doctor` and `key create` warn whenever a seat's owner holds more than one
  live inference key.

## A seat should look like one person

Owning a seat is half of it; the traffic should also look like that one
person's, because the provider sees only the traffic. What the gateway does:

- **One session per conversation.** A Codex seat sends one `session_id` for a
  whole conversation, derived from the conversation's sticky key, as the
  owner's own CLI does — not a fresh one per request, which made one seat look
  like hundreds of sessions an hour.
- **One reader per seat.** Every replica runs the usage poller, but each seat's
  quota and model list is read by one of them per interval (a Redis claim), at
  a jittered time — not by every replica on its own clock.
- **One name.** The quota read and the token refresh send the same configured
  `originator`/`user-agent` as inference, instead of a second name of their own.
- **Two in flight.** A seat imported from now on defaults to
  `--max-concurrency 2`; eight at once from one plan is a crowd. Raise it with
  the flag if the owner truly runs more. A seat imported earlier keeps the value
  it was stored with — usually 8 — and there is no CLI setter for it yet, so
  lower it with `UPDATE account SET max_concurrency = 2 WHERE name = '<seat>'`.

What it cannot do on its own: every replica calls upstream from its own
address. Running more than one replica, give each seat a `proxy_url` so its
traffic leaves from one place, as its owner's does. The column is honoured by
inference, refresh and the usage poll alike, but nothing in the CLI sets it
yet: `UPDATE account SET proxy_url = 'http://proxy.internal:3128' WHERE name =
'<seat>'`.

## A plain endpoint cannot reach a provider's own API

An endpoint — an upstream you register rather than one built in; see
[03-providers.md](03-providers.md#registered-endpoints) — files its keys as
ordinary pooled `api_key` credentials, and nothing reads what such a key is.
Pointed at a provider this gateway already serves, an endpoint would be a way
around every rule above: a Claude subscription token filed as a key under an
endpoint named `claude-direct` would reach `api.anthropic.com` past the refusal
migration 0018 puts on the `anthropic` provider, and a ChatGPT or Grok seat's
token would serve a pool instead of its one owner.

So an endpoint on the `plain` platform may not have a base URL whose host is, or
is under, any of these:

| Hosts | Served instead by |
|---|---|
| `anthropic.com`, `claude.ai`, `claude.com` | the built-in `anthropic` provider, for API keys; a Claude subscription is never served |
| `chatgpt.com`, `openai.com` | `openai` for API keys; a Codex seat through `--from codex`, bound to its owner |
| `x.ai`, `grok.com` | `xai` for API keys; a Grok seat through `--from grok`, bound to its owner |
| `googleapis.com`, `amazonaws.com`, `azure.com` | the built-in Gemini and Bedrock providers, and the `gcp`, `aws` and `azure` platforms, which sign a request the way each cloud expects |

The host is compared as a URL parser reads it, so case, a trailing dot, a port,
percent-encoding and a full-width dot do not get around it, and a name that only
ends in the same letters (`notopenai.com`) is not under one of these. An IP
literal carries no name, so the guard cannot say whose address it is and does
not apply to one; the link-local and cloud-metadata refusal still does. Loopback
and private addresses stay allowed, so a model server on your own network can be
registered. A server of your own reachable only at a cloud's hostname under one
of these domains (an AWS load balancer's `*.elb.amazonaws.com`, say) needs a DNS
name of its own to be registered as a plain endpoint.

The gateway applies the list every time it loads endpoints, not only when one is
written: the schema does not know it, so a row written by hand is refused when it
is loaded. A refused row serves nothing, is logged with the reason on every
refresh and counted in `oag_endpoint_invalid_total{reason="compliance"}`, and
`oag admin account add` will not file a key under it.

What no URL check can see is a proxy you run that forwards to one of these
hosts. Traffic through it is yours to keep within the rules above.

## Practical guidance

If you want colleagues to reach frontier models through this gateway, the two
clean paths are:

1. **Console API keys** pooled for the org. Simplest, explicitly permitted, and
   the reason `api_key` is the default kind.
2. **Team or Enterprise seats**, one per person, bound with
   `owner_principal_id`. Often cheaper than everyone holding an individual Max
   subscription, and it is what those plans are for.

Both give you the whole cost engine: tier ladders, classification, escalation,
budgets, and savings reporting all work the same regardless of credential kind.

## What is deliberately absent

TLS fingerprint impersonation, HTTP header mimicry, client-identity rewriting,
and stripping steganographic markers from prompts are absent. That machinery
exists to hide traffic a provider is trying to detect, and it is an arms race
with no end.

None of it is here. An internal gateway on sanctioned credentials has nothing to
hide, so the default build links no BoringSSL and ships no impersonation code.
The `Transport` trait in `oag-upstream` leaves the seam open, because "we do not
need this" and "this is impossible to add" are different claims and only the
first is true.
