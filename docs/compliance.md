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
| `bedrock`, `vertex` | **Sanctioned**, governed by your cloud agreement. | Deployments already on AWS or GCP. |
| `oauth` — Team/Enterprise seat | **Sanctioned.** OAuth covers Free, Pro, Max, Team, and Enterprise purchasers. | Per-person binding: each member signs in with their own seat. |
| `oauth` — individual Pro/Max seat, shared | **Constrained.** These plans assume ordinary, individual usage. | Personal single-user deployments. |
| `service_account` | Depends on the provider. | Provider-specific. |

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
  its routes. Correct for `api_key`, `bedrock`, and `vertex`.

The scheduler and the router do not care which. Everything else in this
repository works identically either way.

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

sub2api carries a large subsystem for TLS fingerprint impersonation, HTTP header
mimicry, client-identity rewriting, and stripping steganographic markers from
prompts. That exists because resold subscription traffic gets detected, and it
is an arms race with no end.

None of it is here. An internal gateway on sanctioned credentials has nothing to
hide, so the default build links no BoringSSL and ships no impersonation code.
The `Transport` trait in `oag-upstream` leaves the seam open, because "we do not
need this" and "this is impossible to add" are different claims and only the
first is true.
