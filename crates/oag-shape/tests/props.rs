//! Invariances from PUA spec §6.2.
#![allow(clippy::unwrap_used)]

use oag_shape::{ShapeFeatures, shape_of};
use proptest::prelude::*;
use unicode_normalization::UnicodeNormalization as _;

fn prose_move_space(s: &str) -> String {
    if let Some(i) = s.find(' ') {
        let mut t = s.to_owned();
        t.insert(i, ' ');
        t
    } else {
        s.to_owned()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn nfc_nfd_and_prose_space_preserve_counts(
        s in "[a-zA-Z ,.\\n`]{0,80}"
    ) {
        let base = shape_of(&s);
        let nfd: String = s.nfd().collect();
        let spaced = prose_move_space(&s);
        prop_assert_eq!(shape_of(&nfd).fenced_blocks, base.fenced_blocks);
        prop_assert_eq!(shape_of(&nfd).diff_hunks, base.diff_hunks);
        prop_assert_eq!(shape_of(&spaced).fenced_blocks, base.fenced_blocks);
        prop_assert_eq!(shape_of(&spaced).diff_hunks, base.diff_hunks);
    }

    #[test]
    fn json_key_order_does_not_change_key_count(
        keys in proptest::collection::btree_set("[a-z]{1,4}", 1..6)
    ) {
        let mut map = serde_json::Map::new();
        for (i, k) in keys.iter().enumerate() {
            map.insert(k.clone(), serde_json::json!(i));
        }
        let forward = serde_json::Value::Object(map.clone()).to_string();
        let rev: serde_json::Map<_, _> = map.into_iter().rev().collect();
        let backward = serde_json::Value::Object(rev).to_string();
        prop_assert_eq!(shape_of(&forward).json_keys, shape_of(&backward).json_keys);
        prop_assert_eq!(shape_of(&forward).json_keys, u32::try_from(keys.len()).unwrap_or(u32::MAX));
    }

    #[test]
    fn never_panics(s in "\\PC{0,200}") {
        let _: ShapeFeatures = shape_of(&s);
    }
}
