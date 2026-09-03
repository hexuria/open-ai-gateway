//! Turning the model name a client sent into the one the router knows.
//!
//! Two decorations can ride on an inbound model id, and both are stripped here
//! so that exactly one place in the process understands them:
//!
//! - a leading `anthropic/`, which is Claude Code's discovery filter talking
//!   (see below), and
//! - a trailing `@api` / `@sub`, which pins the request to a credential kind.
//!
//! They compose, because they answer different questions and a client that
//! picked `anthropic/xai/grok-4.6@sub` out of our own listing must be able to
//! send it back: `anthropic/xai/grok-4.6@sub` is `xai/grok-4.6` on a
//! subscription. The qualifier comes off first — it is a suffix, the prefix is
//! a prefix, and the catalog lookup in the middle must see neither.
//!
//! **A different upstream is a different provider; the same upstream on a
//! different credential is a qualifier.** Gemini resold by Cursor is a
//! different base URL, adapter, auth and bill, so it is `cursor/gemini-flash`
//! and needs no syntax of its own. One upstream reached by two credential kinds
//! is the only case a qualifier is for. The unqualified form stays the default
//! and is the whole product: three seats and an API key are four paths to one
//! model, and picking the cheapest live one is what the router is for. A
//! qualifier is for the caller who has a reason to override that.
//!
//! ## Claude Code's gateway model discovery, and the id shape it forces on us
//!
//! With `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` the CLI GETs this
//! gateway's `/v1/models`, caches the answer at `~/.claude/cache/gateway-models.json`
//! as `{ baseUrl, fetchedAt, models: [{ id, display_name }] }`, and then
//! **discards every id that does not match `/^(claude|anthropic)/i`**. It is not
//! a warning and not a sort order: a gateway whose ids are `xai/grok-4.6` and
//! `oag/auto` populates an empty picker, with nothing anywhere saying why.
//!
//! So an id has to start with `anthropic/` to be seen at all. That is a
//! transport detail of one client, not a truth about the model, which is why
//! the prefix is only ever *added* to a second copy of the entry and why
//! `display_name` carries what a human should read.
//!
//! The inverse — accepting a prefixed id back on inference — is not gated on
//! anything. A cache written while the listing was aliased must keep working
//! after the operator turns the listing off, and an id the client picked out of
//! our own listing failing on the next turn is the worst possible time to find
//! out.

use oag_core::credential::CredentialKind;
use oag_core::{Error, Result};
use oag_router::Catalog;

/// What an id has to start with to survive the filter. Lower-case because that
/// is how every canonical id in this catalog is spelled; the filter itself is
/// case-insensitive.
const DISCOVERY_PREFIX: &str = "anthropic/";

/// What separates a model from the credential kind it is pinned to.
///
/// `@` and not `:`, which the Gemini path already spends on the action
/// (`…/models/{model}:generateContent`, split on the *last* colon), and not
/// `/`, which is the provider separator. `@` is an RFC 3986 sub-delim, so it is
/// legal unescaped in a path segment and needs no encoding — and a client that
/// percent-encodes it anyway arrives here decoded.
const CHANNEL_SEPARATOR: char = '@';

/// The virtual namespace. Not in the catalog — `oag/auto` and `oag/<rung>` are
/// synthesised by the router — so a catalog lookup alone cannot tell an aliased
/// virtual name from a typo.
const VIRTUAL_PREFIX: &str = "oag/";

/// Whether Claude Code would keep this id.
#[must_use]
pub(crate) fn survives_discovery_filter(id: &str) -> bool {
    starts_with_ci(id, "claude") || starts_with_ci(id, "anthropic")
}

/// The extra id to advertise this model under, or `None` when it already
/// passes.
///
/// Checking first is what keeps `anthropic/claude-opus-5` from being advertised
/// as `anthropic/anthropic/claude-opus-5` — a name that resolves to nothing and
/// that the picker would happily show anyway.
#[must_use]
pub(crate) fn discovery_alias(id: &str) -> Option<String> {
    (!survives_discovery_filter(id)).then(|| format!("{DISCOVERY_PREFIX}{id}"))
}

/// The id that names this model on one credential kind, or `None` for a kind
/// nobody can address.
///
/// The listing's half of the qualifier. Built here rather than formatted at the
/// call site so that the string a client is offered and the string this module
/// parses back cannot be spelled differently.
#[must_use]
pub(crate) fn qualified_id(id: &str, kind: CredentialKind) -> Option<String> {
    kind.qualifier()
        .map(|q| format!("{id}{CHANNEL_SEPARATOR}{q}"))
}

/// What an inbound model name turned out to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Normalised {
    /// The name the router should see, when it differs from what arrived.
    /// `None` means the string was already canonical — leave it alone.
    pub model: Option<String>,
    /// The credential kind the caller pinned the request to, if any.
    pub channel: Option<CredentialKind>,
}

/// Strip both decorations off an inbound model name.
///
/// The one place a model string is interpreted. Everything downstream — the
/// virtual-name check, the passthrough lookup, credential selection, the ledger
/// — sees a canonical id and, separately, the pin; none of them has to know
/// either spelling exists.
///
/// Errors rather than shrugging when the qualifier is unreadable or impossible.
/// A dropped pin is not a smaller failure than a rejected request: it sends the
/// request to the very credential the caller wrote the pin to exclude, bills
/// the wrong pocket, and says nothing.
pub(crate) fn normalise(model: &str, catalog: &Catalog) -> Result<Normalised> {
    let (base, channel) = split_channel(model, catalog)?;

    // The qualifier comes off before the prefix is examined, so the catalog
    // lookups inside `canonicalise` see a bare id — which is what makes
    // `anthropic/xai/grok-4.6@sub` resolve to `xai/grok-4.6` on a subscription
    // rather than to nothing.
    let canonical = canonicalise(base, catalog).unwrap_or_else(|| base.to_owned());

    if let Some(kind) = channel
        && let Some(spec) = catalog.resolve(&canonical)
        && !spec.provider.support().accepts(kind)
    {
        // Known model, impossible pin. Refused here rather than left to
        // selection, because selection would report a missing credential —
        // which reads as "add one", and there is nothing to add.
        return Err(Error::ChannelNotOffered {
            provider: spec.provider,
            kind,
        });
    }

    Ok(Normalised {
        model: (canonical != model).then_some(canonical),
        channel,
    })
}

/// Split a trailing `@kind` off a model name.
///
/// The catalog wins over the split: an id that genuinely contains an `@` names
/// a model, and reading its tail as a qualifier would silently address
/// something else. Deliberately `catalog.resolve` rather than [`known`] — the
/// virtual namespace is a prefix test, so `oag/auto@sub` would pass `known`
/// intact and route as a rung literally called `auto@sub`.
fn split_channel<'a>(
    model: &'a str,
    catalog: &Catalog,
) -> Result<(&'a str, Option<CredentialKind>)> {
    let Some((base, qualifier)) = model.rsplit_once(CHANNEL_SEPARATOR) else {
        return Ok((model, None));
    };
    if catalog.resolve(model).is_some() {
        return Ok((model, None));
    }
    let kind =
        CredentialKind::from_qualifier(qualifier).ok_or_else(|| Error::UnknownModelChannel {
            qualifier: qualifier.to_owned(),
        })?;
    Ok((base, Some(kind)))
}

/// Turn a discovery alias back into the name the router knows, or `None` to
/// leave the string alone.
///
/// The order is the whole of the correctness here. `anthropic/claude-opus-5` is
/// a real canonical id *and* looks exactly like an alias of `claude-opus-5`, so
/// the full string is tried first and the stripped form is only ever reached
/// when the full one names nothing. Stripping first would resolve a real
/// Anthropic model through its bare upstream name and, in a catalog holding two
/// providers' spellings of it, could land on the wrong one.
///
/// A prefix that strips down to nothing recognisable is left intact, so an
/// unknown model is still reported as unknown rather than being quietly turned
/// into a different request.
#[must_use]
fn canonicalise(model: &str, catalog: &Catalog) -> Option<String> {
    if known(model, catalog) {
        return None;
    }
    let rest = model.strip_prefix(DISCOVERY_PREFIX)?;
    known(rest, catalog).then(|| rest.to_owned())
}

/// Whether the router can do something with this name.
///
/// The virtual arm is not a shortcut: `oag/auto` is synthesised rather than
/// catalogued, and an unknown rung is deliberately treated as `auto` by
/// `plan_request`, so the whole namespace is "known" here.
fn known(model: &str, catalog: &Catalog) -> bool {
    model.starts_with(VIRTUAL_PREFIX) || catalog.resolve(model).is_some()
}

/// ASCII-case-insensitive prefix test that does not allocate.
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oag_core::Provider;
    use oag_core::{TierName, tier::RoutingMode};
    use oag_router::catalog::{Capabilities, ModelId, ModelSpec, Pricing};
    use oag_router::ladder::Rung;
    use oag_router::{Budgets, RoutingPolicy, TierLadder};
    use rust_decimal::Decimal;
    use std::collections::HashSet;

    fn spec(id: &str, provider: Provider, upstream: &str) -> ModelSpec {
        ModelSpec {
            id: ModelId::new(id),
            provider,
            upstream_name: upstream.to_owned(),
            pricing: Pricing {
                input_per_mtok: Decimal::from(1),
                output_per_mtok: Decimal::from(5),
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 200_000,
            max_output_tokens: 64_000,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                reasoning: true,
                prompt_cache: true,
            },
            display_label: None,
        }
    }

    fn catalog() -> Catalog {
        Catalog::from_entries([
            spec(
                "anthropic/claude-opus-5",
                Provider::Anthropic,
                "claude-opus-5",
            ),
            spec("xai/grok-4.6", Provider::XAI, "grok-4.6"),
            spec("gemini/gemini-3-pro", Provider::Gemini, "gemini-3-pro"),
        ])
    }

    /// What `normalise` did to a name, as the two things a caller cares about.
    fn normalised(model: &str) -> (String, Option<CredentialKind>) {
        let n = normalise(model, &catalog()).expect("normalises");
        (n.model.unwrap_or_else(|| model.to_owned()), n.channel)
    }

    /// The old prefix-only helper, kept for the tests that are about the
    /// prefix alone.
    fn canonical_of(model: &str) -> Option<String> {
        normalise(model, &catalog()).expect("normalises").model
    }

    /// The filter as Claude Code spells it, written independently of the
    /// implementation so the two cannot drift together.
    fn claude_code_keeps(id: &str) -> bool {
        let lower = id.to_lowercase();
        lower.starts_with("claude") || lower.starts_with("anthropic")
    }

    #[test]
    fn every_id_the_listing_aliases_survives_claude_codes_filter() {
        // The whole point of the feature. An id that fails here is dropped from
        // the picker with no diagnostic anywhere.
        for id in [
            "xai/grok-4.6",
            "oag/auto",
            "oag/cheap",
            "kimi/kimi-k2",
            "openai/gpt-5",
            "gemini/gemini-3-pro",
        ] {
            let aliased = discovery_alias(id).expect("not already passing");
            assert!(claude_code_keeps(&aliased), "{id} -> {aliased}");
        }
    }

    #[test]
    fn an_id_that_already_passes_is_not_double_prefixed() {
        // `anthropic/anthropic/claude-opus-5` resolves to nothing, and the
        // picker would show it anyway.
        assert_eq!(discovery_alias("anthropic/claude-opus-5"), None);
        assert_eq!(discovery_alias("claude-opus-5"), None);
        // Case-insensitively, because that is how the CLI's regex is written.
        assert_eq!(discovery_alias("Anthropic/Claude-Opus-5"), None);
    }

    #[test]
    fn an_aliased_id_names_the_same_model_as_its_canonical_form() {
        let catalog = catalog();
        let alias = "anthropic/xai/grok-4.6";
        let canonical = canonical_of(alias).expect("stripped");
        assert_eq!(canonical, "xai/grok-4.6");
        assert_eq!(
            catalog.resolve(&canonical).map(|s| s.id.as_str()),
            catalog.resolve("xai/grok-4.6").map(|s| s.id.as_str())
        );
    }

    #[test]
    fn an_aliased_virtual_name_is_still_the_virtual_route() {
        // `oag/auto` is synthesised, not catalogued, so a catalog-only check
        // would leave the alias intact and route it as an unknown model.
        assert_eq!(
            canonical_of("anthropic/oag/auto").as_deref(),
            Some("oag/auto")
        );
        assert_eq!(
            canonical_of("anthropic/oag/cheap").as_deref(),
            Some("oag/cheap")
        );
        // And the stripped form is what `virtual_tier` reads, so `auto` still
        // pins no rung while a named rung still does.
        assert_eq!(super::super::virtual_tier("oag/auto"), None);
        assert_eq!(
            super::super::virtual_tier("oag/cheap"),
            Some(TierName::new("cheap"))
        );
    }

    #[test]
    fn a_real_anthropic_model_is_not_mistaken_for_an_alias_of_itself() {
        // The ambiguity trap: this is both a canonical id and a plausible
        // alias of the bare upstream name. Resolving the full string first is
        // what settles it.
        assert_eq!(canonical_of("anthropic/claude-opus-5"), None);
    }

    #[test]
    fn a_model_that_resolves_to_nothing_stays_unknown() {
        // Stripping unconditionally would turn a typo into a *different*
        // model's request, or into a 400 blaming the wrong name.
        assert_eq!(canonical_of("anthropic/nope/not-a-model"), None);
        assert_eq!(canonical_of("nope/not-a-model"), None);
    }

    #[test]
    fn routing_an_alias_decides_on_the_canonical_id_the_ledger_records() {
        // The ledger writes `decision.model.id`. If the alias survived into the
        // decision, one model's spend would split across two names.
        let catalog = catalog();
        let ladder = TierLadder::new(vec![Rung {
            name: TierName::new("frontier"),
            models: vec![ModelId::new("xai/grok-4.6")],
        }])
        .expect("non-empty");
        let policy =
            RoutingPolicy::new(ladder, Box::new(oag_router::HeuristicClassifier::default()));

        let requested = "anthropic/xai/grok-4.6@sub";
        let normalised = normalise(requested, &catalog)
            .expect("normalises")
            .model
            .expect("stripped");
        let decision = policy
            .decide(
                &RoutingMode::Passthrough,
                Some(&normalised),
                &oag_router::RequestSignal::default(),
                &Budgets::principal_only(oag_router::BudgetState::unlimited(Decimal::ZERO)),
                &catalog,
                1024,
                &HashSet::new(),
            )
            .expect("routable");
        assert_eq!(decision.model.id.as_str(), "xai/grok-4.6");
        assert_eq!(decision.model.upstream_name, "grok-4.6");
    }

    #[test]
    fn every_spelling_of_one_model_decides_on_the_id_the_ledger_records() {
        // `usage_event.model_id` is a join key. Four ways of naming one model
        // must all reach the same id, or one model's spend splits across four
        // names and no report can add them back up. The channel that served it
        // is recorded separately, as `account_id`.
        let catalog = catalog();
        let ladder = TierLadder::new(vec![Rung {
            name: TierName::new("frontier"),
            models: vec![ModelId::new("xai/grok-4.6")],
        }])
        .expect("non-empty");
        let policy =
            RoutingPolicy::new(ladder, Box::new(oag_router::HeuristicClassifier::default()));

        for requested in [
            "xai/grok-4.6",
            "xai/grok-4.6@sub",
            "xai/grok-4.6@api",
            "anthropic/xai/grok-4.6@sub",
        ] {
            let n = normalise(requested, &catalog).expect("normalises");
            let name = n.model.unwrap_or_else(|| requested.to_owned());
            let decision = policy
                .decide(
                    &RoutingMode::Passthrough,
                    Some(&name),
                    &oag_router::RequestSignal::default(),
                    &Budgets::principal_only(oag_router::BudgetState::unlimited(Decimal::ZERO)),
                    &catalog,
                    1024,
                    &HashSet::new(),
                )
                .expect("routable");
            assert_eq!(decision.model.id.as_str(), "xai/grok-4.6", "{requested}");
            // And the upstream is told the model's own name, not ours.
            assert_eq!(decision.model.upstream_name, "grok-4.6", "{requested}");
        }
    }

    #[test]
    fn a_qualifier_names_the_credential_kind_and_leaves_the_model_alone() {
        // The pin's whole job: the same model, on a stated channel. If the
        // qualifier survived into the name, it would be a model nobody has.
        assert_eq!(
            normalised("xai/grok-4.6@sub"),
            ("xai/grok-4.6".to_owned(), Some(CredentialKind::OAuth))
        );
        assert_eq!(
            normalised("xai/grok-4.6@api"),
            ("xai/grok-4.6".to_owned(), Some(CredentialKind::ApiKey))
        );
    }

    #[test]
    fn an_unqualified_name_pins_nothing_which_is_what_makes_the_router_useful() {
        // Three seats and an API key are four paths to one model, and choosing
        // the cheapest live one is the product. The qualifier is the override,
        // never the default.
        assert_eq!(
            normalised("xai/grok-4.6"),
            ("xai/grok-4.6".to_owned(), None)
        );
        assert_eq!(normalised("oag/auto"), ("oag/auto".to_owned(), None));
    }

    #[test]
    fn a_discovery_prefix_and_a_qualifier_compose() {
        // Claude Code will only keep `anthropic/xai/grok-4.6@sub` out of the
        // listing, and sends exactly that string back. Handling one decoration
        // but not the other would make every pinned model unroutable from the
        // one client the prefix exists for.
        assert_eq!(
            normalised("anthropic/xai/grok-4.6@sub"),
            ("xai/grok-4.6".to_owned(), Some(CredentialKind::OAuth))
        );
        assert_eq!(
            normalised("anthropic/oag/auto@api"),
            ("oag/auto".to_owned(), Some(CredentialKind::ApiKey))
        );
        // And an id that already passes the filter keeps composing.
        assert_eq!(
            normalised("anthropic/claude-opus-5@api"),
            (
                "anthropic/claude-opus-5".to_owned(),
                Some(CredentialKind::ApiKey)
            )
        );
    }

    #[test]
    fn a_qualifier_on_a_virtual_name_is_not_read_as_part_of_the_rung() {
        // `known` is a prefix test for the `oag/` namespace, so an unsplit
        // `oag/cheap@sub` would route as a rung literally called `cheap@sub`
        // — which `plan_request` treats as `auto`, silently ignoring both the
        // rung and the pin.
        let (model, channel) = normalised("oag/cheap@sub");
        assert_eq!(model, "oag/cheap");
        assert_eq!(channel, Some(CredentialKind::OAuth));
        assert_eq!(
            super::super::virtual_tier(&model),
            Some(TierName::new("cheap"))
        );
    }

    #[test]
    fn an_unknown_qualifier_is_refused_by_a_message_naming_the_real_ones() {
        // Silently dropping it would send the request to the credential the
        // caller wrote the qualifier to exclude, with nothing anywhere saying
        // so — the exact failure this feature exists to prevent.
        let err = normalise("xai/grok-4.6@bogus", &catalog()).expect_err("refused");
        let message = err.to_string();
        assert!(message.contains("@bogus"), "{message}");
        assert!(message.contains("@api"), "{message}");
        assert!(message.contains("@sub"), "{message}");
        assert!(matches!(err, Error::UnknownModelChannel { .. }));

        // Including the spellings that are nearly right, which is what a person
        // guessing actually types.
        for near in ["@oauth", "@subscription", "@api_key", "@"] {
            let model = format!("xai/grok-4.6{near}");
            assert!(
                normalise(&model, &catalog()).is_err(),
                "{model} was accepted"
            );
        }
    }

    #[test]
    fn a_qualifier_the_provider_cannot_offer_says_so_rather_than_hunting_for_it() {
        // Gemini takes an API key and nothing else. Letting this through would
        // report a missing credential, which reads as "go and add one" — and
        // there is nothing to add.
        let err = normalise("gemini/gemini-3-pro@sub", &catalog()).expect_err("refused");
        assert!(matches!(
            err,
            Error::ChannelNotOffered {
                provider: Provider::Gemini,
                kind: CredentialKind::OAuth,
            }
        ));
        assert!(err.to_string().contains("subscription"), "{err}");

        // The same provider's API key is fine, so this is about the pairing and
        // not about qualifiers in general.
        assert_eq!(
            normalised("gemini/gemini-3-pro@api"),
            (
                "gemini/gemini-3-pro".to_owned(),
                Some(CredentialKind::ApiKey)
            )
        );
    }

    #[test]
    fn the_listing_spells_a_qualified_id_the_way_the_parser_reads_it() {
        // The two halves of the feature, checked against each other: an id this
        // module offers must be one it can take back.
        for kind in CredentialKind::QUALIFIED.iter().copied() {
            let id = qualified_id("xai/grok-4.6", kind).expect("addressable");
            assert_eq!(
                normalised(&id),
                ("xai/grok-4.6".to_owned(), Some(kind)),
                "{id}"
            );
        }
        assert_eq!(qualified_id("xai/grok-4.6", CredentialKind::Bedrock), None);
    }
}
