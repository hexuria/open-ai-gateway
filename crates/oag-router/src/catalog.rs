//! What models exist, what they cost, and what they can do.
//!
//! Seeded from `LiteLLM`'s `model_prices_and_context_window.json`, which is the
//! most complete public pricing table and is what sub2api uses too. We vendor
//! it as a build asset and refresh it with a command, rather than sub2api's
//! background download-and-hash-check service — pricing changes a few times a
//! year, and a gateway that cannot start because a GitHub fetch failed is a
//! worse trade than a table that is occasionally a week stale.

use oag_core::Provider;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// A canonical model identifier, `provider/name`.
///
/// Distinct from the provider's own name for the model, which appears on the
/// wire: Bedrock calls Sonnet `anthropic.claude-sonnet-4-v1:0`, and we should
/// not make routing policy spell that.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ModelId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Per-million-token prices, in USD.
///
/// `Decimal`, never `f64`. These values get multiplied by token counts and
/// summed across millions of rows; binary floating point accumulates visible
/// drift and there is no reason to accept it for a fixed-point quantity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_mtok: Decimal,
    pub output_per_mtok: Decimal,
    /// Reading a cached prefix. Typically ~10% of input. The single largest
    /// lever on agentic workloads, where most of the prompt repeats every turn.
    #[serde(default)]
    pub cache_read_per_mtok: Option<Decimal>,
    /// Writing a prefix into the cache. Typically ~125% of input.
    #[serde(default)]
    pub cache_write_per_mtok: Option<Decimal>,
}

/// What a model can be asked to do.
///
/// Used to reject a rung rather than discover the incapability as a 400 from
/// upstream: routing a vision request to a text-only model is a decision we can
/// make correctly for free.
// Independent feature flags, not a state machine: a model can have any subset
// of these and no combination is invalid.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub vision: bool,
    pub tools: bool,
    /// Extended thinking / reasoning budgets.
    pub reasoning: bool,
    pub prompt_cache: bool,
}

/// One model, as routing policy sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: ModelId,
    pub provider: Provider,
    /// What to actually put on the wire for this provider.
    pub upstream_name: String,
    pub pricing: Pricing,
    pub context_window: u32,
    pub max_output_tokens: u32,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// What an operator named this model, if anyone has.
    ///
    /// An id is an address — clients send it, rungs name it, the ledger records
    /// it — so renaming one breaks all three. A label is a name and renaming a
    /// name is free. Keeping them as one string is why renaming has felt
    /// dangerous, so this is deliberately a second field rather than a
    /// mutable id.
    ///
    /// `None` is the common case and means "derive it": see [`ModelSpec::label`].
    /// `#[serde(default)]` because a serialised catalog written before this
    /// field existed must still load.
    #[serde(default)]
    pub display_label: Option<String>,
}

/// What a human should read for a model, as opposed to what the router needs.
///
/// The vendor's own spelling plus the name on the wire. Derived, not invented:
/// a marketing name we made up would disagree with the provider's own console
/// the first time they rename something. Public and shared so the listing, the
/// admin API and the dashboard placeholder all show the same string — three
/// copies of one format is three chances for a rename to look like it did
/// nothing.
#[must_use]
pub fn derive_label(vendor: &str, name: &str) -> String {
    format!("{vendor}: {name}")
}

impl ModelSpec {
    /// What a picker should show for this model.
    ///
    /// The operator's label when there is one, the derived default otherwise.
    /// Falling back rather than backfilling the column means an unnamed model
    /// follows the provider's own spelling forever, and a named one is exactly
    /// what someone typed.
    #[must_use]
    pub fn label(&self) -> String {
        self.display_label.clone().unwrap_or_else(|| {
            derive_label(self.provider.support().display_name, &self.upstream_name)
        })
    }

    /// Whether this model can serve a request with these requirements.
    #[must_use]
    pub fn satisfies(&self, need: &Requirements) -> bool {
        if need.vision && !self.capabilities.vision {
            return false;
        }
        if need.tools && !self.capabilities.tools {
            return false;
        }
        if need.reasoning && !self.capabilities.reasoning {
            return false;
        }
        // Leave headroom for the response: a prompt that exactly fills the
        // window leaves nowhere to answer.
        let needed = need
            .prompt_tokens
            .saturating_add(u64::from(need.max_output_tokens));
        needed <= u64::from(self.context_window)
    }
}

/// What a specific request needs from whatever model serves it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requirements {
    pub prompt_tokens: u64,
    pub max_output_tokens: u32,
    pub vision: bool,
    pub tools: bool,
    pub reasoning: bool,
}

/// Every model the gateway knows about.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    models: BTreeMap<ModelId, ModelSpec>,
    /// Every catalogue id sharing an upstream name — one, usually; two when
    /// two providers spell a model identically, which is the ambiguity
    /// `resolve` refuses to guess at. Maintained by `insert`, so the two
    /// lookups the request path makes by upstream name are a probe each
    /// rather than a scan of the catalogue.
    by_upstream: std::collections::HashMap<String, Vec<ModelId>>,
}

impl Catalog {
    /// The dearest model in this catalogue whose upstream name is in `served`.
    ///
    /// The savings baseline: what these tokens would have cost on the best
    /// model the caller could actually have reached. Filtered by `served`
    /// rather than taken from a ladder, because a rung can name a model the
    /// credential refuses — comparing against something unreachable is the
    /// same class of untruth as advertising it.
    ///
    /// "Dearest" is input plus output per Mtok, a single scalar because the
    /// token split is not known when the baseline is chosen. In practice that
    /// makes it "dearest by output", since output typically runs five to six
    /// times input; it only differs from a more careful ranking when two
    /// candidates straddle. Pricing the baseline at meter time, where the real
    /// split is known, would remove the arbitrariness — a later change.
    ///
    /// An unpriced row cannot win, so a model the catalogue has not priced
    /// never becomes the baseline and silently reports a saving of zero.
    ///
    /// `need` is applied INSIDE the selection, not to its result. Filtering
    /// afterwards would discard the whole served set whenever the single
    /// dearest model could not hold the prompt, falling back to the ladder
    /// exactly on the largest requests — the ones whose counterfactual matters
    /// most — while a cheaper, capable, still-dearer-than-the-rung model sat
    /// right there. `None` therefore means "nothing served can do this at
    /// all", which is what the caller's fallback should be reacting to.
    #[must_use]
    pub fn dearest_served(
        &self,
        served: &std::collections::HashSet<String>,
        need: &Requirements,
    ) -> Option<&ModelSpec> {
        // Iterate the served set — a handful of names — and probe, rather
        // than walk a few hundred catalogue rows asking each whether it is in
        // the set. This ran on every request, including the common one where
        // `served` was empty and the answer was provably `None` before the
        // walk began.
        if served.is_empty() {
            return None;
        }
        served
            .iter()
            .filter_map(|name| self.by_upstream.get(name))
            .flatten()
            .filter_map(|id| self.models.get(id))
            .filter(|spec| spec.satisfies(need))
            .filter(|spec| {
                spec.pricing.input_per_mtok > rust_decimal::Decimal::ZERO
                    || spec.pricing.output_per_mtok > rust_decimal::Decimal::ZERO
            })
            // Ties broken by id, because the walk is over a `HashSet` and
            // `max_by_key` keeps whichever equal element it met last: the
            // baseline — and the `counterfactual_model` the ledger records —
            // has to be the same for the same inputs.
            .max_by(|a, b| {
                (a.pricing.input_per_mtok + a.pricing.output_per_mtok)
                    .cmp(&(b.pricing.input_per_mtok + b.pricing.output_per_mtok))
                    .then_with(|| b.id.as_str().cmp(a.id.as_str()))
            })
    }

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, spec: ModelSpec) {
        // A re-insert under the same id may carry a different upstream name
        // (a reprice, a rename); drop the old mapping so it cannot resolve to
        // a spec that no longer spells it.
        if let Some(previous) = self.models.get(&spec.id)
            && previous.upstream_name != spec.upstream_name
            && let Some(ids) = self.by_upstream.get_mut(&previous.upstream_name)
        {
            ids.retain(|id| *id != spec.id);
        }
        let ids = self
            .by_upstream
            .entry(spec.upstream_name.clone())
            .or_default();
        if !ids.contains(&spec.id) {
            ids.push(spec.id.clone());
        }
        self.models.insert(spec.id.clone(), spec);
    }

    #[must_use]
    pub fn get(&self, id: &ModelId) -> Option<&ModelSpec> {
        self.models.get(id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModelSpec> {
        self.models.values()
    }

    /// Resolve a name a client sent us.
    ///
    /// Accepts the canonical `provider/name` and, as a convenience, a bare
    /// upstream name when it is unambiguous. Ambiguity resolves to `None`
    /// rather than guessing: silently picking one of two providers for
    /// `gpt-5` would route spend somewhere the operator did not choose.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&ModelSpec> {
        if let Some(spec) = self.models.get(&ModelId::new(name)) {
            return Some(spec);
        }
        // Exactly one catalogue id spells this upstream name, or nothing:
        // two is the ambiguity the doc above refuses to guess at.
        match self.by_upstream.get(name).map(Vec::as_slice) {
            Some([only]) => self.models.get(only),
            _ => None,
        }
    }

    /// Build from parsed `LiteLLM` pricing entries.
    pub fn from_entries(entries: impl IntoIterator<Item = ModelSpec>) -> Self {
        let mut catalog = Self::new();
        for spec in entries {
            catalog.insert(spec);
        }
        catalog
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    fn spec(id: &str, provider: Provider, upstream: &str, ctx: u32) -> ModelSpec {
        ModelSpec {
            id: ModelId::new(id),
            provider,
            upstream_name: upstream.to_owned(),
            pricing: Pricing {
                input_per_mtok: dec!(1),
                output_per_mtok: dec!(5),
                cache_read_per_mtok: Some(dec!(0.1)),
                cache_write_per_mtok: Some(dec!(1.25)),
            },
            context_window: ctx,
            max_output_tokens: 8192,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                reasoning: false,
                prompt_cache: true,
            },
            display_label: None,
        }
    }

    fn priced(id: &str, upstream: &str, input: Decimal, output: Decimal) -> ModelSpec {
        let mut s = spec(id, Provider::OpenAI, upstream, 200_000);
        s.pricing.input_per_mtok = input;
        s.pricing.output_per_mtok = output;
        s
    }

    #[test]
    fn the_dearest_served_model_is_the_savings_baseline() {
        let catalog = Catalog::from_entries([
            priced("openai/cheap", "cheap", dec!(0.2), dec!(1.2)),
            priced("openai/dear", "dear", dec!(5), dec!(30)),
            // Dearer still, but the credential does not serve it — comparing
            // against something unreachable is the untruth being removed.
            priced("openai/unreachable", "unreachable", dec!(50), dec!(300)),
        ]);
        let served: std::collections::HashSet<String> =
            ["cheap", "dear"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            catalog
                .dearest_served(&served, &Requirements::default())
                .expect("a match")
                .id
                .as_str(),
            "openai/dear"
        );
    }

    #[test]
    fn a_price_tie_resolves_the_same_way_every_time() {
        // The served set is a `HashSet`, so without an explicit tie-break the
        // baseline depended on iteration order and the ledger recorded whichever
        // model won that draw.
        //
        // The set is rebuilt on every pass, which is the whole point. `HashSet`
        // draws its hash seed once, when it is constructed — so asking the same
        // set sixteen times asks one question sixteen times, and this test used
        // to detect a missing tie-break only when that single draw happened to
        // put the wrong twin first. About half the time, decided at process
        // start, which is the worst kind of test: it fails for people whose
        // build is fine and passes for people whose build is not.
        //
        // A fresh set per pass is a fresh draw, so thirty-two of them miss a
        // missing tie-break with probability 2^-32 rather than 1/2.
        let catalog = Catalog::from_entries([
            priced("openai/b-twin", "b-twin", dec!(5), dec!(30)),
            priced("openai/a-twin", "a-twin", dec!(5), dec!(30)),
        ]);
        for pass in 0..32 {
            let served: std::collections::HashSet<String> = ["a-twin", "b-twin"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            assert_eq!(
                catalog
                    .dearest_served(&served, &Requirements::default())
                    .expect("a match")
                    .id
                    .as_str(),
                "openai/a-twin",
                "pass {pass}: the baseline is the ledger's `counterfactual_model`, \
                 so equal prices have to resolve to the same model every time"
            );
        }
    }

    #[test]
    fn an_unpriced_model_never_becomes_the_baseline() {
        // A zero-priced row would report a saving of zero for every request
        // measured against it, which reads as "the gateway saved you nothing"
        // rather than as "we could not say".
        let catalog = Catalog::from_entries([
            priced("openai/cheap", "cheap", dec!(0.2), dec!(1.2)),
            priced("openai/unpriced", "unpriced", dec!(0), dec!(0)),
        ]);
        let served: std::collections::HashSet<String> = ["cheap", "unpriced"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            catalog
                .dearest_served(&served, &Requirements::default())
                .expect("a match")
                .id
                .as_str(),
            "openai/cheap"
        );
    }

    #[test]
    fn nothing_served_means_no_baseline_rather_than_a_wrong_one() {
        let catalog = Catalog::from_entries([priced("openai/a", "a", dec!(1), dec!(2))]);
        assert!(
            catalog
                .dearest_served(&std::collections::HashSet::new(), &Requirements::default())
                .is_none(),
            "an empty served set must not fall through to an arbitrary model"
        );
    }

    #[test]
    fn ambiguous_bare_names_do_not_resolve() {
        let catalog = Catalog::from_entries([
            spec("openai/gpt-5", Provider::OpenAI, "gpt-5", 400_000),
            spec("vertex/gpt-5", Provider::Vertex, "gpt-5", 400_000),
        ]);
        // Guessing here would route spend to a provider the operator did not pick.
        assert!(catalog.resolve("gpt-5").is_none());
        assert!(catalog.resolve("openai/gpt-5").is_some());
    }

    #[test]
    fn context_check_leaves_room_for_the_answer() {
        let model = spec("m/small", Provider::Kimi, "small", 8_000);
        let need = Requirements {
            prompt_tokens: 7_000,
            max_output_tokens: 4_000,
            ..Requirements::default()
        };
        // 7k prompt fits in 8k, but not alongside a 4k response.
        assert!(!model.satisfies(&need));
    }

    #[test]
    fn missing_capability_disqualifies() {
        let mut model = spec("m/text", Provider::Kimi, "text", 100_000);
        model.capabilities.vision = false;
        let need = Requirements {
            prompt_tokens: 10,
            vision: true,
            ..Requirements::default()
        };
        assert!(!model.satisfies(&need));
    }
}
