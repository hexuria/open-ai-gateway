//! Claude Code's gateway model discovery, and the id shape it forces on us.
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

use oag_router::Catalog;

/// What an id has to start with to survive the filter. Lower-case because that
/// is how every canonical id in this catalog is spelled; the filter itself is
/// case-insensitive.
const DISCOVERY_PREFIX: &str = "anthropic/";

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
pub(crate) fn canonicalise(model: &str, catalog: &Catalog) -> Option<String> {
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
        ])
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
        let canonical = canonicalise(alias, &catalog).expect("stripped");
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
        let catalog = catalog();
        assert_eq!(
            canonicalise("anthropic/oag/auto", &catalog).as_deref(),
            Some("oag/auto")
        );
        assert_eq!(
            canonicalise("anthropic/oag/cheap", &catalog).as_deref(),
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
        let catalog = catalog();
        assert_eq!(canonicalise("anthropic/claude-opus-5", &catalog), None);
    }

    #[test]
    fn a_model_that_resolves_to_nothing_stays_unknown() {
        // Stripping unconditionally would turn a typo into a *different*
        // model's request, or into a 400 blaming the wrong name.
        let catalog = catalog();
        assert_eq!(canonicalise("anthropic/nope/not-a-model", &catalog), None);
        assert_eq!(canonicalise("nope/not-a-model", &catalog), None);
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

        let requested = "anthropic/xai/grok-4.6";
        let normalised = canonicalise(requested, &catalog).expect("stripped");
        let decision = policy
            .decide(
                &RoutingMode::Passthrough,
                Some(&normalised),
                &oag_router::RequestSignal::default(),
                &Budgets::principal_only(oag_router::BudgetState::unlimited(Decimal::ZERO)),
                &catalog,
                1024,
            )
            .expect("routable");
        assert_eq!(decision.model.id.as_str(), "xai/grok-4.6");
        assert_eq!(decision.model.upstream_name, "grok-4.6");
    }
}
