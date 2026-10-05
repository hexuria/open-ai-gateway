#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn fences_and_diffs() {
    let text = "intro\n```rust\nfn main() {}\n```\nout\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-a\n+b\n@@ -5 +5 @@\n-c\n+d\n";
    let f = shape_of(text);
    assert_eq!(f.fenced_blocks, 1);
    assert_eq!(f.diff_hunks, 2);
}

#[test]
fn dominant_script_and_homoglyphs() {
    let latin = shape_of("hello world");
    assert_eq!(latin.dominant_script, DominantScript::Latin);
    assert_eq!(latin.homoglyphs, 0);
    let cyr = shape_of("привет");
    assert_eq!(cyr.dominant_script, DominantScript::Cyrillic);
    // Cyrillic 'ѕ' + Latin "top"
    let homo = shape_of("\u{0455}top the build");
    assert!(homo.homoglyphs >= 1, "{homo:?}");
}

#[test]
fn json_keys_and_structured_output() {
    let plain = shape_of(r#"{"a":1,"b":{"c":2}}"#);
    assert_eq!(plain.json_keys, 3);
    assert!(!plain.structured_output);
    let structured = shape_of(r#"{"model":"x","response_format":{"type":"json_schema"}}"#);
    assert!(structured.structured_output);
    assert!(structured.json_keys >= 3);
    let prose = shape_of("please set response_format to json");
    assert!(prose.structured_output);
    assert_eq!(prose.json_keys, 0);
}

#[test]
fn no_tier_type_in_the_public_api() {
    // Compile-time: ShapeFeatures fields are the whole surface.
    let f = ShapeFeatures {
        fenced_blocks: 0,
        diff_hunks: 0,
        dominant_script: DominantScript::None,
        homoglyphs: 0,
        json_keys: 0,
        structured_output: false,
    };
    assert_eq!(f.fingerprint()[17], 0);
    assert_eq!(DominantScript::Latin.name(), "latin");
}

#[test]
fn unterminated_fence_counts() {
    let f = shape_of("before ```code without close");
    assert_eq!(f.fenced_blocks, 1);
}

#[test]
fn mutants_survivors_pinned() {
    // Display writes the stable name (not a silent Ok(())).
    assert_eq!(DominantScript::Latin.to_string(), "latin");
    assert_eq!(DominantScript::None.to_string(), "none");
    assert_eq!(DominantScript::Greek.to_string(), "greek");

    // Fingerprint always includes the "shape1" tag — never all zeros.
    let empty = ShapeFeatures {
        fenced_blocks: 0,
        diff_hunks: 0,
        dominant_script: DominantScript::None,
        homoglyphs: 0,
        json_keys: 0,
        structured_output: false,
    };
    assert_ne!(empty.fingerprint(), [0u8; 24]);
    let tagged = shape_of("```\nx\n```");
    assert_ne!(tagged.fingerprint(), [0u8; 24]);
    assert_eq!(&tagged.fingerprint()[18..24], b"shape1");

    // Empty / digits-only → None (Common/Inherited ignored; `c > 0` not `>= 0`).
    assert_eq!(shape_of("").dominant_script, DominantScript::None);
    assert_eq!(shape_of("123 456").dominant_script, DominantScript::None);
    assert_eq!(shape_of("!!!").dominant_script, DominantScript::None);

    // Per-script arms.
    assert_eq!(shape_of("αβγ").dominant_script, DominantScript::Greek);
    assert_eq!(shape_of("漢字").dominant_script, DominantScript::Han);
    assert_eq!(
        shape_of("ひらがな").dominant_script,
        DominantScript::Japanese
    );
    assert_eq!(
        shape_of("カタカナ").dominant_script,
        DominantScript::Japanese
    );
    assert_eq!(shape_of("한글").dominant_script, DominantScript::Korean);
    assert_eq!(shape_of("العربية").dominant_script, DominantScript::Arabic);
    assert_eq!(shape_of("עברית").dominant_script, DominantScript::Hebrew);
    assert_eq!(script_of('a'), Some(DominantScript::Latin));
    assert_eq!(script_of('1'), None);
    assert_eq!(script_of('\u{0301}'), None); // combining acute = Inherited

    // Hunk header: either `@@ ` form or compact `@@…@@` form (OR, not AND).
    assert!(is_hunk_header("@@ -1 +1 @@"));
    assert!(is_hunk_header("@@-1,1 +1,1@@"));
    assert!(!is_hunk_header("@ @ -1 +1 @@"));
    assert!(!is_hunk_header("not a hunk"));

    // Raw helpers (normalize Err path); pin arithmetic and filters.
    assert_eq!(count_fences_raw("```a``` ```b```"), 2);
    assert_eq!(count_fences_raw("```unterminated"), 1);
    assert_eq!(count_fences_raw("no fences"), 0);
    assert!(count_homoglyphs_raw("\u{0455}top latin") >= 1);
    assert_eq!(count_homoglyphs_raw("plain latin"), 0);
    assert_eq!(dominant_raw("αβ"), DominantScript::Greek);
    assert_eq!(dominant_raw("123"), DominantScript::None);
    assert_eq!(dominant_raw("hello"), DominantScript::Latin);

    // JSON arrays contribute nested keys; structured OR arms.
    let arr = shape_of(r#"[{"response_format":{"type":"json"}}]"#);
    assert!(arr.structured_output);
    assert!(arr.json_keys >= 2);
    assert!(text_asks_structured("please use JSON_SCHEMA here"));
    assert!(text_asks_structured("structured_outputs please"));
    assert!(!text_asks_structured("plain prose"));

    // Equal script counts: first script wins (strict > / saturating_sub).
    assert_eq!(dominant_raw("aα"), DominantScript::Latin);
    assert_eq!(dominant_raw("αa"), DominantScript::Greek);

    // Each structured JSON key alone flips the flag (walk_json arms are independent).
    assert!(shape_of(r#"{"json_schema":{}}"#).structured_output);
    assert!(shape_of(r#"{"structured_outputs":true}"#).structured_output);
    assert!(!shape_of(r#"{"other":1}"#).structured_output);

    // Fence pairs with content between openers/closers (strip_prefix path).
    assert_eq!(count_fences_raw("x```ab```y```cd```z"), 2);
}
