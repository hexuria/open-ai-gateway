//! Reasoning effort: the levels a model takes for `reasoning_effort`, and the
//! one it uses when a request sends none.
//!
//! `/v1/models` publishes them in opencodex's shape, so a client's effort
//! slider shows only the stops a model has, and no slider for a model whose
//! levels are not known. They come from two places, both read when the
//! catalog is written and never per request:
//!
//! - OpenRouter's public model list, whose `reasoning.supported_efforts` and
//!   `reasoning.default_effort` cover a few hundred models
//!   (`oag_upstream::openrouter` reads it into a [`Snapshot`]). A catalog row
//!   matches an entry by its model's name at the vendor: see [`vendor_name`].
//! - [`Overrides`]: `reasoning-efforts.json`, committed beside this crate, for
//!   the rows this deployment serves most, stating what their own serving path
//!   takes. An entry wins over OpenRouter's, an entry saying a model takes no
//!   level included.
//!
//! The override table is applied when a row is written, by `oag admin
//! endpoint sync` and `oag admin catalog sync-efforts`, so a row stores what
//! `/v1/models` serves and the gateway reads nothing but the row. Editing the
//! table changes nothing until one of those runs.
//!
//! Nothing is guessed. A level not in [`LEVELS`] cannot be put in order, so a
//! list naming one is not used, and nor is a list whose default it does not
//! hold.

use oag_core::{Error, Provider, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// One level: what a request sends, and what a picker shows for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub value: &'static str,
    /// opencodex's words: the value with its first letter capitalised, then
    /// ` Effort`.
    pub label: &'static str,
}

/// Every level known, lowest first, which is the order a slider shows them in.
///
/// OpenRouter's vocabulary, `none` to `max`, and `ultra`, which the Codex
/// backend's own catalog lists above `max`.
pub const LEVELS: [Level; 8] = [
    Level {
        value: "none",
        label: "None Effort",
    },
    Level {
        value: "minimal",
        label: "Minimal Effort",
    },
    Level {
        value: "low",
        label: "Low Effort",
    },
    Level {
        value: "medium",
        label: "Medium Effort",
    },
    Level {
        value: "high",
        label: "High Effort",
    },
    Level {
        value: "xhigh",
        label: "Xhigh Effort",
    },
    Level {
        value: "max",
        label: "Max Effort",
    },
    Level {
        value: "ultra",
        label: "Ultra Effort",
    },
];

/// A model's levels, lowest first, and its default.
///
/// Only [`ReasoningEfforts::new`] makes one, so there is always at least one
/// level, each known and listed once, and the default is one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningEfforts {
    levels: Vec<Level>,
    default: Level,
}

impl ReasoningEfforts {
    /// `levels`, in any order, with `default`: `None` unless every level is in
    /// [`LEVELS`] and `default` is one of them. They come out lowest first
    /// whatever order they went in, each once: OpenRouter lists them highest
    /// first.
    #[must_use]
    pub fn new<'a>(levels: impl IntoIterator<Item = &'a str>, default: &str) -> Option<Self> {
        let given: Vec<&str> = levels.into_iter().collect();
        if !given
            .iter()
            .all(|value| LEVELS.iter().any(|known| known.value == *value))
        {
            return None;
        }
        let levels: Vec<Level> = LEVELS
            .into_iter()
            .filter(|known| given.contains(&known.value))
            .collect();
        let default = levels.iter().copied().find(|l| l.value == default)?;
        Some(Self { levels, default })
    }

    /// Lowest first.
    #[must_use]
    pub fn levels(&self) -> &[Level] {
        &self.levels
    }

    #[must_use]
    pub fn default_level(&self) -> Level {
        self.default
    }

    /// The levels' values, lowest first: what a catalog row stores.
    #[must_use]
    pub fn values(&self) -> Vec<&'static str> {
        self.levels.iter().map(|l| l.value).collect()
    }
}

/// The override table committed beside this crate.
const COMMITTED: &str = include_str!("../reasoning-efforts.json");

/// Levels stated for particular catalog rows, by id, which win over
/// OpenRouter's. `None` states that the model takes no level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides(HashMap<String, Option<ReasoningEfforts>>);

/// One entry, as the file writes it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stated {
    /// Any order; stored lowest first. Empty says the model takes no level.
    efforts: Vec<String>,
    /// One of `efforts`, and absent when that is empty.
    #[serde(default)]
    default: Option<String>,
    /// Where the levels come from. An entry nobody can trace is a guess.
    source: String,
}

impl Overrides {
    /// The committed table, `reasoning-efforts.json`.
    pub fn committed() -> Result<Self> {
        Self::parse(COMMITTED)
    }

    /// A table in the committed file's shape, keyed by catalog id, or why one
    /// of its entries cannot be used.
    pub fn parse(raw: &str) -> Result<Self> {
        let stated: HashMap<String, Stated> = serde_json::from_str(raw).map_err(|e| {
            Error::Config(format!(
                "the reasoning-effort overrides are not a table of entries: {e}"
            ))
        })?;
        let mut table = HashMap::new();
        for (id, entry) in stated {
            if entry.source.trim().is_empty() {
                return Err(Error::Config(format!(
                    "the reasoning-effort override for {id} names no source"
                )));
            }
            let efforts = match (entry.efforts.is_empty(), entry.default.as_deref()) {
                (true, None) => None,
                (false, Some(default)) => Some(
                    ReasoningEfforts::new(entry.efforts.iter().map(String::as_str), default)
                        .ok_or_else(|| {
                            Error::Config(format!(
                                "the reasoning-effort override for {id} names a level outside \
                                 {}, or a default it does not list",
                                LEVELS.map(|l| l.value).join(", ")
                            ))
                        })?,
                ),
                _ => {
                    return Err(Error::Config(format!(
                        "the reasoning-effort override for {id} names levels and a default \
                         among them, or neither"
                    )));
                }
            };
            table.insert(id, efforts);
        }
        Ok(Self(table))
    }

    /// The entry for one catalog id: `Some(None)` when the table says the
    /// model takes no level, `None` when it says nothing.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Option<ReasoningEfforts>> {
        self.0.get(id)
    }
}

/// OpenRouter's levels, by each model's name at its vendor as matching spells
/// it ([`vendor_name`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot(HashMap<String, ReasoningEfforts>);

impl Snapshot {
    /// From OpenRouter's ids (`x-ai/grok-4.6`) and the levels each states. An
    /// id naming no vendor can match no row, and is left out; where two ids
    /// spell one model, the first listed is kept.
    pub fn new(listed: impl IntoIterator<Item = (String, ReasoningEfforts)>) -> Self {
        let mut snapshot = HashMap::new();
        for (id, efforts) in listed {
            if let Some(name) = canonical(&id) {
                snapshot.entry(name).or_insert(efforts);
            }
        }
        Self(snapshot)
    }

    /// How many models it holds levels for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Vendors with more than one name, each beside the one matching uses:
/// OpenRouter's (`x-ai`, `z-ai`, `moonshotai`, `mistralai`, `meta-llama`), and
/// the built-in providers' (`zhipu`, `kimi`, `gemini`), where Merge Gateway and
/// the like spell the vendor otherwise.
const VENDORS: [(&str, &str); 8] = [
    ("x-ai", "xai"),
    ("z-ai", "zai"),
    ("zhipu", "zai"),
    ("moonshotai", "moonshot"),
    ("kimi", "moonshot"),
    ("gemini", "google"),
    ("mistralai", "mistral"),
    ("meta-llama", "meta"),
];

/// `vendor/model`, with the vendor named as [`VENDORS`] names it, or `None`
/// when `name` names no vendor.
fn canonical(name: &str) -> Option<String> {
    let (vendor, model) = name.split_once('/')?;
    let vendor = VENDORS
        .iter()
        .find(|(spelt, _)| *spelt == vendor)
        .map_or(vendor, |(_, one)| one);
    Some(format!("{vendor}/{model}"))
}

/// The name a catalog row's model goes by at its vendor, as matching spells
/// it, or `None` when the row names no vendor.
///
/// A built-in provider's row is its own id: `xai/grok-4.6` is OpenRouter's
/// `x-ai/grok-4.6`. An endpoint's row is its id less the endpoint's name:
/// `merge/zai/glm-5.3-flash` is `zai/glm-5.3-flash`, OpenRouter's
/// `z-ai/glm-5.3-flash`, and an endpoint whose own name for the model has no
/// slash names no vendor.
#[must_use]
pub fn vendor_name(id: &str, provider: &str) -> Option<String> {
    if Provider::ALL.iter().any(|p| p.as_str() == provider) {
        canonical(id)
    } else {
        canonical(id.split_once('/')?.1)
    }
}

/// How many rows took their levels from where.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Named by the override table, which may say the model takes no level.
    pub overridden: usize,
    /// Listed by OpenRouter.
    pub listed: usize,
    /// Named by neither: no levels are known, so none are published.
    pub unknown: usize,
}

/// The levels each catalog row, `(id, provider)`, is to store, and where they
/// came from: the override table's entry for its id, else OpenRouter's for
/// the model it names, else none.
pub fn plan<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a str)>,
    overrides: &Overrides,
    openrouter: &Snapshot,
) -> (Vec<(String, Option<ReasoningEfforts>)>, Tally) {
    let mut tally = Tally::default();
    let levels = rows
        .into_iter()
        .map(|(id, provider)| {
            let efforts = if let Some(stated) = overrides.get(id) {
                tally.overridden += 1;
                stated.clone()
            } else if let Some(listed) =
                vendor_name(id, provider).and_then(|name| openrouter.0.get(&name))
            {
                tally.listed += 1;
                Some(listed.clone())
            } else {
                tally.unknown += 1;
                None
            };
            (id.to_owned(), efforts)
        })
        .collect();
    (levels, tally)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn efforts(levels: &[&str], default: &str) -> ReasoningEfforts {
        ReasoningEfforts::new(levels.iter().copied(), default).expect("valid levels")
    }

    #[test]
    fn levels_listed_highest_first_come_out_lowest_first() {
        // GPT-5.6 Sol, exactly as OpenRouter lists it.
        let sol = efforts(&["max", "xhigh", "high", "medium", "low", "none"], "medium");
        assert_eq!(
            sol.values(),
            ["none", "low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(sol.default_level().value, "medium");
        // And any other order lands the same way.
        assert_eq!(
            efforts(&["high", "low", "max"], "max").values(),
            ["low", "high", "max"]
        );
        assert_eq!(
            efforts(&["ultra", "minimal"], "ultra").values(),
            ["minimal", "ultra"]
        );
    }

    #[test]
    fn every_level_is_labelled_in_opencodexs_words() {
        // opencodex: `${value[0].toUpperCase()}${value.slice(1)} Effort`.
        for level in LEVELS {
            let mut chars = level.value.chars();
            let first = chars.next().expect("a value").to_uppercase();
            assert_eq!(
                level.label,
                format!("{first}{} Effort", chars.as_str()),
                "{level:?}"
            );
        }
        assert_eq!(
            LEVELS.map(|l| l.value),
            [
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
            ],
            "lowest first"
        );
        let labels: Vec<&str> = efforts(&["xhigh", "low"], "low")
            .levels()
            .iter()
            .map(|l| l.label)
            .collect();
        assert_eq!(labels, ["Low Effort", "Xhigh Effort"]);
    }

    #[test]
    fn a_list_that_cannot_be_ordered_or_has_no_default_is_not_used() {
        // A level nobody can place in the order is not guessed at.
        assert_eq!(
            ReasoningEfforts::new(["low", "extreme", "high"], "low"),
            None
        );
        // A default the list does not hold.
        assert_eq!(ReasoningEfforts::new(["low", "high"], "medium"), None);
        // Nothing at all.
        assert_eq!(ReasoningEfforts::new([], "medium"), None);
        // Listed twice is listed once.
        assert_eq!(
            efforts(&["high", "low", "high"], "high").values(),
            ["low", "high"]
        );
    }

    #[test]
    fn the_committed_overrides_are_usable_and_every_entry_is_read() {
        // `parse` refuses an entry with an unknown level, a stray default or
        // no source, so parsing at all is most of the check. The rest is that
        // nothing in the file was dropped on the way.
        let committed = Overrides::committed().expect("the committed table parses");
        let raw: serde_json::Value = serde_json::from_str(COMMITTED).expect("JSON");
        let ids: Vec<&String> = raw.as_object().expect("an object").keys().collect();
        assert!(!ids.is_empty(), "the committed table names some rows");
        for id in ids {
            assert!(committed.get(id).is_some(), "{id} was read");
        }
        assert_eq!(
            committed.0.len(),
            raw.as_object().map_or(0, serde_json::Map::len)
        );
    }

    #[test]
    fn an_override_states_levels_or_none_and_nothing_in_between() {
        let table = Overrides::parse(
            r#"{
                "a/one": {"efforts": ["high", "low"], "default": "low", "source": "a doc"},
                "a/off": {"efforts": [], "source": "a probe"}
            }"#,
        )
        .expect("parses");
        assert_eq!(
            table.get("a/one"),
            Some(&Some(efforts(&["low", "high"], "low")))
        );
        assert_eq!(table.get("a/off"), Some(&None), "says it takes no level");
        assert_eq!(table.get("a/unnamed"), None, "says nothing");

        for (raw, says) in [
            (
                r#"{"a/m": {"efforts": ["low"], "source": "x"}}"#,
                "names levels and a default among them, or neither",
            ),
            (
                r#"{"a/m": {"efforts": [], "default": "low", "source": "x"}}"#,
                "names levels and a default among them, or neither",
            ),
            (
                r#"{"a/m": {"efforts": ["low", "turbo"], "default": "low", "source": "x"}}"#,
                "names a level outside none, minimal, low, medium, high, xhigh, max, ultra",
            ),
            (
                r#"{"a/m": {"efforts": ["low"], "default": "high", "source": "x"}}"#,
                "or a default it does not list",
            ),
            (
                r#"{"a/m": {"efforts": ["low"], "default": "low", "source": "  "}}"#,
                "the reasoning-effort override for a/m names no source",
            ),
            (
                r#"{"a/m": {"efforts": ["low"], "default": "low"}}"#,
                "missing field `source`",
            ),
            (
                r#"{"a/m": {"efforts": ["low"], "default": "low", "source": "x", "defualt": 1}}"#,
                "unknown field `defualt`",
            ),
            (r#"["a/m"]"#, "not a table of entries"),
        ] {
            let err = Overrides::parse(raw).expect_err(raw).to_string();
            assert!(err.contains(says), "{raw}: {err}");
        }
    }

    #[test]
    fn a_rows_vendor_name_is_its_id_or_its_id_past_the_endpoint() {
        assert_eq!(
            vendor_name("xai/grok-4.6", "xai").as_deref(),
            Some("xai/grok-4.6")
        );
        assert_eq!(
            vendor_name("zhipu/glm-5.3", "zhipu").as_deref(),
            Some("zai/glm-5.3"),
            "a built-in's name for its vendor, spelt as matching spells it"
        );
        assert_eq!(
            vendor_name("merge/zai/glm-5.3-flash", "merge").as_deref(),
            Some("zai/glm-5.3-flash")
        );
        assert_eq!(
            vendor_name("merge/openai/gpt-5.5", "merge").as_deref(),
            Some("openai/gpt-5.5")
        );
        assert_eq!(
            vendor_name("groq/llama-4-scout", "groq"),
            None,
            "an endpoint's own model name with no vendor in it"
        );
        assert_eq!(vendor_name("merge", "merge"), None);
    }

    #[test]
    fn every_spelling_of_a_vendor_matches_the_same_model() {
        for (ours, theirs) in [
            ("xai/grok-4.6", "x-ai/grok-4.6"),
            ("zai/glm-5.3", "z-ai/glm-5.3"),
            ("zhipu/glm-5.3", "z-ai/glm-5.3"),
            ("moonshot/kimi-k3", "moonshotai/kimi-k3"),
            ("kimi/kimi-k3", "moonshotai/kimi-k3"),
            ("gemini/gemini-3.8-flash", "google/gemini-3.8-flash"),
            ("mistral/mistral-large", "mistralai/mistral-large"),
            ("meta/llama-4", "meta-llama/llama-4"),
            ("openai/gpt-5.5", "openai/gpt-5.5"),
        ] {
            assert_eq!(canonical(ours), canonical(theirs), "{ours} is {theirs}");
        }
        assert_ne!(canonical("xai/grok-4.6"), canonical("xai/grok-4.7"));
        assert_eq!(canonical("no-vendor"), None);
    }

    fn snapshot() -> Snapshot {
        Snapshot::new([
            (
                "x-ai/grok-4.6".to_owned(),
                efforts(&["xhigh", "high", "medium", "low"], "high"),
            ),
            (
                "z-ai/glm-5.3-flash".to_owned(),
                efforts(&["max", "high", "low"], "max"),
            ),
            (
                "openai/gpt-5.5".to_owned(),
                efforts(&["xhigh", "high", "medium", "low", "none"], "medium"),
            ),
            // A second spelling of a model already listed: the first stays.
            ("xai/grok-4.6".to_owned(), efforts(&["low"], "low")),
            ("unvendored".to_owned(), efforts(&["low"], "low")),
        ])
    }

    #[test]
    fn a_snapshot_keeps_the_first_spelling_and_only_vendored_names() {
        let snapshot = snapshot();
        assert_eq!(snapshot.len(), 3);
        assert!(!snapshot.is_empty());
        assert_eq!(
            snapshot.0.get("xai/grok-4.6"),
            Some(&efforts(&["low", "medium", "high", "xhigh"], "high"))
        );
        assert!(Snapshot::new([]).is_empty());
        assert_eq!(Snapshot::new([]).len(), 0);
    }

    #[test]
    fn an_override_beats_openrouter_and_a_row_neither_names_has_no_levels() {
        let overrides = Overrides::parse(
            r#"{
                "openai/gpt-5.5": {"efforts": ["low", "medium", "high", "xhigh"],
                                   "default": "medium", "source": "the seat's catalog"},
                "xai/grok-4.6": {"efforts": [], "source": "a probe"}
            }"#,
        )
        .expect("parses");
        let (levels, tally) = plan(
            [
                ("openai/gpt-5.5", "openai"),
                ("xai/grok-4.6", "xai"),
                ("merge/zai/glm-5.3-flash", "merge"),
                ("merge/openai/gpt-5.5", "merge"),
                ("anthropic/claude-haiku-4.5", "anthropic"),
                ("groq/llama-4-scout", "groq"),
            ],
            &overrides,
            &snapshot(),
        );
        assert_eq!(
            levels,
            [
                (
                    "openai/gpt-5.5".to_owned(),
                    Some(efforts(&["low", "medium", "high", "xhigh"], "medium")),
                ),
                ("xai/grok-4.6".to_owned(), None),
                (
                    "merge/zai/glm-5.3-flash".to_owned(),
                    Some(efforts(&["low", "high", "max"], "max")),
                ),
                (
                    "merge/openai/gpt-5.5".to_owned(),
                    Some(efforts(
                        &["none", "low", "medium", "high", "xhigh"],
                        "medium"
                    )),
                ),
                ("anthropic/claude-haiku-4.5".to_owned(), None),
                ("groq/llama-4-scout".to_owned(), None),
            ],
            "the override is the row's own, so Merge's resale of the model keeps OpenRouter's"
        );
        assert_eq!(
            tally,
            Tally {
                overridden: 2,
                listed: 2,
                unknown: 2
            }
        );
    }
}
