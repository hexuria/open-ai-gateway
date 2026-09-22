//! OpenAI function-name sanitisation.
//!
//! Chat Completions and Responses both validate `tools[i].name` against
//! `^[a-zA-Z0-9_-]{1,64}$`. Anthropic's documented pattern is the same shape,
//! but clients still send names that fail it — MCP/connector tools in
//! particular (`user-Github.get_file`, `namespace.tool`, a path, a name with
//! spaces). Lenient OpenAI-compatible upstreams used to accept those; luna
//! does not, and the whole turn comes back 400.
//!
//! Canonical keeps the client's names. Only the OpenAI-shaped *outbound*
//! codecs rewrite them for the wire, and a [`FunctionNameMap`] puts the
//! original back on `tool_calls` so the client can still dispatch.

use crate::canonical::{CanonicalRequest, ContentBlock, ToolChoice};
use crate::stream::StreamEvent;
use std::collections::{HashMap, HashSet};

/// The OpenAI function-name ceiling, in bytes. After sanitisation every
/// character is ASCII, so this is also the character ceiling.
pub const OPENAI_FUNCTION_NAME_MAX: usize = 64;

/// What a name that sanitises to nothing is sent as.
const FALLBACK: &str = "tool";

/// Whether `name` is already legal on the OpenAI function-name wire.
#[must_use]
pub fn is_legal_openai_function_name(name: &str) -> bool {
    let len = name.len();
    (1..=OPENAI_FUNCTION_NAME_MAX).contains(&len)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Strip/replace anything the OpenAI pattern refuses, then cap at 64.
///
/// Illegal characters (dots, spaces, slashes, colons, everything else) become
/// `_`, consecutive underscores collapse, and leading/trailing underscores
/// are trimmed so a name that was only punctuation does not become `_`. An
/// empty result is [`FALLBACK`], not an empty string — the pattern's `{1,64}`
/// refuses that too.
#[must_use]
pub fn sanitize_openai_function_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(OPENAI_FUNCTION_NAME_MAX));
    for b in name.bytes() {
        let ok = b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        if ok {
            out.push(char::from(b));
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    let stem = if trimmed.is_empty() {
        FALLBACK
    } else {
        trimmed
    };
    truncate_ascii(stem, OPENAI_FUNCTION_NAME_MAX).to_owned()
}

/// Bidirectional original ↔ wire names for one request.
///
/// Built from the canonical tools list (plus a named `tool_choice` and any
/// historical `ToolUse` blocks) so a later `tool_call` that echoes a wire name
/// can be restored to the name the client declared. Identity mappings are
/// stored too: a legal name that another name would have collided with must
/// keep its seat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionNameMap {
    to_wire: HashMap<String, String>,
    from_wire: HashMap<String, String>,
}

impl FunctionNameMap {
    /// No rewrites. `wire`/`original` return their argument unchanged.
    #[must_use]
    pub fn identity() -> Self {
        Self::default()
    }

    /// Map every function name this request will put on an OpenAI wire.
    #[must_use]
    pub fn from_request(req: &CanonicalRequest) -> Self {
        let mut originals = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |name: &str| {
            if seen.insert(name.to_owned()) {
                originals.push(name.to_owned());
            }
        };
        for t in &req.tools {
            push(&t.name);
        }
        if let Some(ToolChoice::Tool { name }) = &req.tool_choice {
            push(name);
        }
        for m in &req.messages {
            for b in &m.content {
                if let ContentBlock::ToolUse { name, .. } = b {
                    push(name);
                }
            }
        }
        Self::from_originals(&originals)
    }

    /// Assign wire names, preferring to leave already-legal names untouched so
    /// a legal `foo_bar` is not displaced by a sanitised `foo.bar`.
    #[must_use]
    pub fn from_originals(originals: &[String]) -> Self {
        let mut used = HashSet::new();
        let mut to_wire = HashMap::new();

        for name in originals {
            if is_legal_openai_function_name(name) && used.insert(name.clone()) {
                to_wire.insert(name.clone(), name.clone());
            }
        }
        for name in originals {
            if to_wire.contains_key(name) {
                continue;
            }
            let wire = uniquify(&sanitize_openai_function_name(name), &used);
            used.insert(wire.clone());
            to_wire.insert(name.clone(), wire);
        }

        let from_wire = to_wire
            .iter()
            .map(|(orig, wire)| (wire.clone(), orig.clone()))
            .collect();
        Self { to_wire, from_wire }
    }

    /// The name to put on the OpenAI wire for this original.
    #[must_use]
    pub fn wire<'a>(&'a self, original: &'a str) -> &'a str {
        self.to_wire.get(original).map_or(original, String::as_str)
    }

    /// The client-facing name a wire name came from.
    #[must_use]
    pub fn original<'a>(&'a self, wire: &'a str) -> &'a str {
        self.from_wire.get(wire).map_or(wire, String::as_str)
    }

    /// Whether any original was rewritten. Same-dialect passthrough is only
    /// safe when this is false — otherwise the client would see the wire
    /// names and fail to dispatch.
    #[must_use]
    pub fn rewrites(&self) -> bool {
        self.to_wire.iter().any(|(k, v)| k != v)
    }

    /// Original → wire pairs that actually changed, for logs.
    #[must_use]
    pub fn rewritten(&self) -> Vec<(&str, &str)> {
        let mut pairs: Vec<(&str, &str)> = self
            .to_wire
            .iter()
            .filter(|(k, v)| k != v)
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        pairs.sort_unstable();
        pairs
    }

    /// Restore client-facing names on parsed tool-call events.
    pub fn restore_in_events(&self, events: &mut [StreamEvent]) {
        if self.from_wire.is_empty() {
            return;
        }
        for event in events {
            if let StreamEvent::ToolUseStart { name, .. } = event {
                *name = self.original(name).to_owned();
            }
        }
    }
}

fn uniquify(base: &str, used: &HashSet<String>) -> String {
    let base = {
        let stem = truncate_ascii(base, OPENAI_FUNCTION_NAME_MAX);
        if stem.is_empty() {
            FALLBACK.to_owned()
        } else {
            stem.to_owned()
        }
    };
    if !used.contains(&base) {
        return base;
    }
    let mut n = 2usize;
    loop {
        let suffix = format!("_{n}");
        let stem_len = OPENAI_FUNCTION_NAME_MAX.saturating_sub(suffix.len());
        let stem = if stem_len == 0 {
            ""
        } else {
            truncate_ascii(&base, stem_len)
        };
        let mut candidate = format!("{stem}{suffix}");
        if candidate.len() > OPENAI_FUNCTION_NAME_MAX {
            candidate.truncate(OPENAI_FUNCTION_NAME_MAX);
        }
        if candidate.is_empty() {
            FALLBACK.clone_into(&mut candidate);
        }
        if !used.contains(&candidate) {
            return candidate;
        }
        if n == usize::MAX {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

fn truncate_ascii(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{CanonicalRequest, ContentBlock, Message, Role, Tool, ToolChoice};
    use serde_json::json;

    fn legal(name: &str) {
        assert!(
            is_legal_openai_function_name(name),
            "{name:?} must match ^[a-zA-Z0-9_-]{{1,64}}$"
        );
    }

    fn req_with_tools(names: &[&str]) -> CanonicalRequest {
        CanonicalRequest {
            model: "m".to_owned(),
            system: vec![],
            messages: vec![],
            tools: names
                .iter()
                .map(|n| Tool {
                    name: (*n).to_owned(),
                    description: String::new(),
                    input_schema: json!({ "type": "object" }),
                    cache_control: None,
                })
                .collect(),
            max_tokens: 256,
            stream: false,
            temperature: None,
            thinking_budget: None,
            thinking_effort: None,
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
        }
    }

    #[test]
    fn illegal_characters_become_underscores() {
        // The luna 400: MCP/connector names with dots, and the rest of the
        // punctuation agents actually send.
        assert_eq!(
            sanitize_openai_function_name("user-Github.get_file"),
            "user-Github_get_file"
        );
        assert_eq!(
            sanitize_openai_function_name("namespace.tool"),
            "namespace_tool"
        );
        assert_eq!(
            sanitize_openai_function_name("user-Notion/query"),
            "user-Notion_query"
        );
        assert_eq!(
            sanitize_openai_function_name("mcp:github:search"),
            "mcp_github_search"
        );
        assert_eq!(sanitize_openai_function_name("read file"), "read_file");
        assert_eq!(
            sanitize_openai_function_name("foo...bar"),
            "foo_bar",
            "consecutive illegal chars collapse to one underscore"
        );
        legal(&sanitize_openai_function_name("user-Github.get_file"));
        legal(&sanitize_openai_function_name("a/b:c d.e"));
    }

    #[test]
    fn a_name_that_sanitises_to_nothing_is_still_non_empty() {
        assert_eq!(sanitize_openai_function_name(""), FALLBACK);
        assert_eq!(sanitize_openai_function_name("..."), FALLBACK);
        assert_eq!(sanitize_openai_function_name(":::"), FALLBACK);
        assert_eq!(sanitize_openai_function_name("   "), FALLBACK);
        legal(FALLBACK);
    }

    #[test]
    fn a_name_longer_than_64_is_truncated() {
        let long = format!("{}X", "a".repeat(64));
        assert_eq!(long.len(), 65);
        let out = sanitize_openai_function_name(&long);
        assert_eq!(out.len(), 64);
        assert_eq!(out, "a".repeat(64));
        legal(&out);

        let dotted = format!("{}.tail", "b".repeat(70));
        let out = sanitize_openai_function_name(&dotted);
        assert_eq!(out.len(), 64);
        legal(&out);
    }

    #[test]
    fn colliding_sanitized_names_stay_distinct() {
        // A legal name keeps its seat; the illegal ones that sanitise onto it
        // get a suffix rather than stealing it.
        let map = FunctionNameMap::from_originals(&[
            "foo_bar".to_owned(),
            "foo.bar".to_owned(),
            "foo/bar".to_owned(),
        ]);
        assert_eq!(map.wire("foo_bar"), "foo_bar");
        assert_eq!(map.wire("foo.bar"), "foo_bar_2");
        assert_eq!(map.wire("foo/bar"), "foo_bar_3");
        legal(map.wire("foo.bar"));
        legal(map.wire("foo/bar"));

        assert_eq!(map.original("foo_bar"), "foo_bar");
        assert_eq!(map.original("foo_bar_2"), "foo.bar");
        assert_eq!(map.original("foo_bar_3"), "foo/bar");
    }

    #[test]
    fn truncated_collisions_are_disambiguated_inside_64() {
        let a = format!("{}.x", "a".repeat(70));
        let b = format!("{}.y", "a".repeat(70));
        let map = FunctionNameMap::from_originals(&[a.clone(), b.clone()]);
        let wa = map.wire(&a);
        let wb = map.wire(&b);
        assert_ne!(wa, wb);
        legal(wa);
        legal(wb);
        assert_eq!(map.original(wa), a);
        assert_eq!(map.original(wb), b);
    }

    #[test]
    fn a_tool_call_round_trips_to_the_client_facing_name() {
        let req = req_with_tools(&[
            "Shell",
            "Grep",
            "Read",
            "Write",
            "StrReplace",
            "Glob",
            // tools[6] on a typical agent list: an MCP connector with a dot.
            "user-Github.get_file",
        ]);
        let map = FunctionNameMap::from_request(&req);
        assert_eq!(map.wire("user-Github.get_file"), "user-Github_get_file");
        assert!(
            map.rewrites(),
            "passthrough must be skipped when a name changed"
        );

        let mut events = vec![StreamEvent::ToolUseStart {
            id: "call_1".to_owned(),
            name: map.wire("user-Github.get_file").to_owned(),
        }];
        map.restore_in_events(&mut events);
        match &events[0] {
            StreamEvent::ToolUseStart { name, .. } => {
                assert_eq!(name, "user-Github.get_file");
            }
            other => panic!("expected ToolUseStart, got {other:?}"),
        }
    }

    #[test]
    fn historical_tool_use_and_named_choice_are_mapped_too() {
        let mut req = req_with_tools(&["read_file"]);
        req.tool_choice = Some(ToolChoice::Tool {
            name: "namespace.tool".to_owned(),
        });
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_owned(),
                name: "user-Github.get_file".to_owned(),
                input: json!({}),
            }],
        });
        let map = FunctionNameMap::from_request(&req);
        assert_eq!(map.wire("read_file"), "read_file");
        assert_eq!(map.wire("namespace.tool"), "namespace_tool");
        assert_eq!(map.wire("user-Github.get_file"), "user-Github_get_file");
        assert_eq!(map.original("namespace_tool"), "namespace.tool");
        assert_eq!(map.original("user-Github_get_file"), "user-Github.get_file");
    }

    #[test]
    fn legal_names_are_left_alone_and_do_not_count_as_rewrites() {
        let map = FunctionNameMap::from_originals(&[
            "read_file".to_owned(),
            "Shell".to_owned(),
            "a-b_c1".to_owned(),
        ]);
        assert!(!map.rewrites());
        assert!(map.rewritten().is_empty());
        assert_eq!(map.wire("read_file"), "read_file");
    }
}
