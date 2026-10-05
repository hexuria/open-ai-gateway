//! `oag-shape`: integer shape features of a gateway request.
//!
//! Moved from `hexuria/pua` `packs/pua-gateway` (PUA architecture audit, Phase 2): the gateway
//! owns what a request's shape *means*; PUA's `pua-text` supplies the Unicode-safe normalization
//! (fences and other protected spans, confusable tokens) this crate counts over.
//!
//! Counts only — fenced blocks, diff hunks, dominant script, homoglyphs, JSON keys,
//! structured-output markers. **No tier type exists in this crate.** Wording is never mapped
//! to a routing tier (`oag-router` owns that).
//!
//! ```
//! use oag_shape::{ShapeFeatures, shape_of};
//!
//! let f = shape_of("see ```rust\nfn main(){}\n``` and a diff:\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n");
//! assert_eq!(f.fenced_blocks, 1);
//! assert!(f.diff_hunks >= 1);
//! assert!(!f.structured_output);
//! ```
#![forbid(unsafe_code)]

use core::fmt;

use pua_text::{NormalizeConfig, ProtectedKind, normalize};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_script::{Script, UnicodeScript};
use unicode_security::skeleton;

/// Algorithm tag for consumers that fold shape features into a `DataVersion`.
pub const ALGORITHM_TAG: &str = "oag-shape/1";

/// A stable script id for the dominant script (Common/Inherited ignored when anything else
/// appears). Values are the Unicode script short names' blake-free ordinals we pin here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
#[non_exhaustive]
pub enum DominantScript {
    /// No letters, or only Common/Inherited.
    None,
    /// Latin.
    Latin,
    /// Cyrillic.
    Cyrillic,
    /// Greek.
    Greek,
    /// Han (CJK ideographs).
    Han,
    /// Hiragana or Katakana.
    Japanese,
    /// Hangul.
    Korean,
    /// Arabic.
    Arabic,
    /// Hebrew.
    Hebrew,
    /// Anything else we don't special-case; the raw Unicode script name is not exposed so a
    /// new Unicode version cannot silently change the public API surface.
    Other,
}

impl DominantScript {
    /// Stable name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Latin => "latin",
            Self::Cyrillic => "cyrillic",
            Self::Greek => "greek",
            Self::Han => "han",
            Self::Japanese => "japanese",
            Self::Korean => "korean",
            Self::Arabic => "arabic",
            Self::Hebrew => "hebrew",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for DominantScript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Integer shape features of one gateway request body / message list (PUA spec §6.2).
///
/// Every field is an integer or a bool. There is deliberately no `tier`, `route`, or
/// `complexity` type in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShapeFeatures {
    /// Number of fenced code blocks (`` ``` `` … `` ``` ``), including unterminated fences
    /// that run to the end of the text.
    pub fenced_blocks: u32,
    /// Number of unified-diff hunk headers (`@@ ... @@`).
    pub diff_hunks: u32,
    /// Dominant letter script (Common/Inherited ignored when a real script is present).
    pub dominant_script: DominantScript,
    /// Tokens whose UTS #39 skeleton differs from the token itself (homoglyph / lookalike).
    pub homoglyphs: u32,
    /// JSON object keys when the input parses as JSON, counting every occurrence at any depth
    /// (a key repeated in two objects counts twice); 0 if it does not parse.
    pub json_keys: u32,
    /// True when the text asks for structured output (`"response_format"`, `"json_schema"`,
    /// or a top-level JSON object with those keys).
    pub structured_output: bool,
}

impl ShapeFeatures {
    /// Fingerprint bytes for `DataVersion` (little-endian integers + flags).
    pub fn fingerprint(self) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[0..4].copy_from_slice(&self.fenced_blocks.to_le_bytes());
        out[4..8].copy_from_slice(&self.diff_hunks.to_le_bytes());
        out[8] = self.dominant_script as u8;
        out[9..13].copy_from_slice(&self.homoglyphs.to_le_bytes());
        out[13..17].copy_from_slice(&self.json_keys.to_le_bytes());
        out[17] = u8::from(self.structured_output);
        out[18..24].copy_from_slice(b"shape1");
        out
    }
}

/// Extracts [`ShapeFeatures`] from a gateway request body or concatenated message text.
pub fn shape_of(text: &str) -> ShapeFeatures {
    let config = NormalizeConfig::default();
    // Normalize may refuse over-long input; fall back to raw counting so a hostile body still
    // yields features instead of panicking the consumer.
    let (fenced_blocks, homoglyphs, dominant_script) = match normalize(text, config) {
        Ok(n) => {
            let fenced = n
                .protected()
                .iter()
                .filter(|p| p.kind() == ProtectedKind::FencedCode)
                .count();
            let mut scripts: [(DominantScript, u32); 9] = [
                (DominantScript::Latin, 0),
                (DominantScript::Cyrillic, 0),
                (DominantScript::Greek, 0),
                (DominantScript::Han, 0),
                (DominantScript::Japanese, 0),
                (DominantScript::Korean, 0),
                (DominantScript::Arabic, 0),
                (DominantScript::Hebrew, 0),
                (DominantScript::Other, 0),
            ];
            let mut homo = 0u32;
            for t in n.tokens() {
                if t.confusable().is_some() {
                    homo = homo.saturating_add(1);
                }
                let word = n.token_text(t);
                for ch in word.chars() {
                    if let Some(slot) = script_of(ch)
                        && let Some((_, c)) = scripts.iter_mut().find(|(s, _)| *s == slot)
                    {
                        *c = c.saturating_add(1);
                    }
                }
            }
            let dominant = scripts
                .iter()
                .max_by_key(|(_, c)| *c)
                .and_then(|(s, c)| (*c > 0).then_some(*s))
                .unwrap_or(DominantScript::None);
            (u32::try_from(fenced).unwrap_or(u32::MAX), homo, dominant)
        }
        Err(_) => (
            count_fences_raw(text),
            count_homoglyphs_raw(text),
            dominant_raw(text),
        ),
    };

    let diff_hunks = text.lines().filter(|l| is_hunk_header(l)).count();
    let (json_keys, structured_from_json) = json_shape(text);
    let structured_output = structured_from_json || text_asks_structured(text);

    ShapeFeatures {
        fenced_blocks,
        diff_hunks: u32::try_from(diff_hunks).unwrap_or(u32::MAX),
        dominant_script,
        homoglyphs,
        json_keys,
        structured_output,
    }
}

pub(crate) fn is_hunk_header(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("@@ ") || (t.starts_with("@@") && t[2..].contains("@@"))
}

pub(crate) fn script_of(ch: char) -> Option<DominantScript> {
    match ch.script() {
        Script::Common | Script::Inherited | Script::Unknown => None,
        Script::Latin => Some(DominantScript::Latin),
        Script::Cyrillic => Some(DominantScript::Cyrillic),
        Script::Greek => Some(DominantScript::Greek),
        Script::Han => Some(DominantScript::Han),
        Script::Hiragana | Script::Katakana => Some(DominantScript::Japanese),
        Script::Hangul => Some(DominantScript::Korean),
        Script::Arabic => Some(DominantScript::Arabic),
        Script::Hebrew => Some(DominantScript::Hebrew),
        _ => Some(DominantScript::Other),
    }
}

pub(crate) fn count_fences_raw(text: &str) -> u32 {
    let mut n = 0u32;
    let mut rest = text;
    while let Some(i) = rest.find("```") {
        n = n.saturating_add(1);
        // Advance past the opener without `+` (mutants rewrite `i + 3` ↔ `i * 3`).
        rest = rest
            .get(i..)
            .and_then(|s| s.strip_prefix("```"))
            .unwrap_or("");
        if let Some(after) = rest.find("```").and_then(|j| rest.get(j..)) {
            rest = after.strip_prefix("```").unwrap_or("");
        } else {
            break;
        }
    }
    n
}

pub(crate) fn count_homoglyphs_raw(text: &str) -> u32 {
    text.split_whitespace()
        .filter(|w| {
            let sk: String = skeleton(w).collect();
            sk != *w && sk.is_ascii()
        })
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

pub(crate) fn dominant_raw(text: &str) -> DominantScript {
    let mut best = DominantScript::None;
    let mut best_n = 0u32;
    let mut counts = [0u32; 10];
    for ch in text.chars() {
        if let Some(s) = script_of(ch) {
            let i = s as usize;
            counts[i] = counts[i].saturating_add(1);
            let c = counts[i];
            // Strict greater: equal counts keep the earlier script (Latin before Greek, …).
            if c.saturating_sub(best_n) > 0 {
                best_n = c;
                best = s;
            }
        }
    }
    best
}

fn json_shape(text: &str) -> (u32, bool) {
    let Ok(v) = serde_json::from_str::<Value>(text.trim()) else {
        return (0, false);
    };
    let mut keys = 0u32;
    let mut structured = false;
    walk_json(&v, &mut keys, &mut structured);
    (keys, structured)
}

fn walk_json(v: &Value, keys: &mut u32, structured: &mut bool) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                *keys = keys.saturating_add(1);
                if k == "response_format" {
                    *structured = true;
                }
                if k == "json_schema" {
                    *structured = true;
                }
                if k == "structured_outputs" {
                    *structured = true;
                }
                walk_json(child, keys, structured);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                walk_json(child, keys, structured);
            }
        }
        _ => {}
    }
}

pub(crate) fn text_asks_structured(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("response_format")
        || lower.contains("json_schema")
        || lower.contains("structured_outputs")
}

#[cfg(test)]
mod tests;
