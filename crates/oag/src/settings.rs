//! Loading configuration: a YAML file, then environment overrides.
//!
//! The override scheme is `OAG_<SECTION>__<FIELD>`, double underscore for
//! nesting — so `OAG_DATABASE__URL` sets `database.url`. That is what lets a
//! container image ship one config file and have the orchestrator inject
//! secrets, which is the only way `security.signing_secret` can be identical
//! across replicas without being committed.

use oag_core::config::Config;
use oag_core::{Error, Result};
use serde_yaml_ng::Value;

const ENV_PREFIX: &str = "OAG_";

/// Read the file if present, apply environment overrides, then validate.
///
/// The file is optional: a container deployment can configure everything
/// through the environment. What is *not* optional is validation — see
/// [`oag_core::config::SecurityConfig::validate`].
pub fn load(path: Option<&str>) -> Result<Config> {
    let mut doc: Value = match path {
        Some(p) => {
            let raw = std::fs::read_to_string(p)
                .map_err(|e| Error::Config(format!("reading {p}: {e}")))?;
            serde_yaml_ng::from_str(&raw).map_err(|e| Error::Config(format!("parsing {p}: {e}")))?
        }
        None => Value::Mapping(serde_yaml_ng::Mapping::new()),
    };

    apply_overrides(&mut doc, std::env::vars());

    let cfg: Config = serde_yaml_ng::from_value(doc)
        .map_err(|e| Error::Config(format!("building config: {e}")))?;
    cfg.validate()?;
    Ok(cfg)
}

/// The top-level sections an override may address.
///
/// Env vars outside this set are ignored rather than rejected. The distinction
/// matters: a typo *in a config file* is a mistake worth failing on, which is
/// why the file is parsed with `deny_unknown_fields`. But the environment is
/// shared with everything else on the machine — `OAG_ACCOUNT_SECRET`,
/// `OAG_CONFIG`, whatever an operator exports next — and treating an unrelated
/// variable as a bad config key means an unrelated variable can stop the
/// gateway from booting.
const SECTIONS: &[&str] = &[
    "server",
    "database",
    "redis",
    "security",
    "gateway",
    "telemetry",
];

/// Apply overrides from any source of key/value pairs.
///
/// Takes an iterator rather than reading `std::env` directly so it is a pure
/// function, and so the tests can exercise it without mutating the process
/// environment — which in edition 2024 is `unsafe` and, in a test binary that
/// runs threads in parallel, genuinely racy.
fn apply_overrides(doc: &mut Value, vars: impl Iterator<Item = (String, String)>) {
    for (key, value) in vars {
        let Some(rest) = key.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let path: Vec<String> = rest.split("__").map(str::to_lowercase).collect();
        let Some(section) = path.first() else {
            continue;
        };
        // Needs a section *and* a field: `OAG_SERVER` alone addresses nothing.
        if !SECTIONS.contains(&section.as_str()) || path.len() < 2 {
            continue;
        }
        set_path(doc, &path, parse_scalar(&value));
    }
}

/// Parse into the narrowest type that fits.
///
/// Environment variables are strings, but the config schema has integers and
/// booleans in it. Without this, `OAG_SERVER__MAX_BODY_BYTES=1000` fails to
/// deserialise with a type error that points at the config file rather than the
/// variable that actually caused it.
fn parse_scalar(raw: &str) -> Value {
    if let Ok(b) = raw.parse::<bool>() {
        return Value::Bool(b);
    }
    if let Ok(i) = raw.parse::<i64>() {
        return Value::Number(i.into());
    }
    Value::String(raw.to_owned())
}

fn set_path(doc: &mut Value, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };

    let mut cursor = doc;
    for segment in parents {
        if !cursor.is_mapping() {
            *cursor = Value::Mapping(serde_yaml_ng::Mapping::new());
        }
        let Some(map) = cursor.as_mapping_mut() else {
            return;
        };
        let key = Value::String(segment.clone());
        cursor = map
            .entry(key)
            .or_insert_with(|| Value::Mapping(serde_yaml_ng::Mapping::new()));
    }

    if !cursor.is_mapping() {
        *cursor = Value::Mapping(serde_yaml_ng::Mapping::new());
    }
    if let Some(map) = cursor.as_mapping_mut() {
        map.insert(Value::String(last.clone()), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> Value {
        serde_yaml_ng::from_str(src).expect("valid yaml")
    }

    #[test]
    fn double_underscore_addresses_a_nested_field() {
        let mut d = doc("database:\n  url: from-file\n  max_connections: 4\n");
        set_path(
            &mut d,
            &["database".to_owned(), "url".to_owned()],
            Value::String("from-env".to_owned()),
        );
        assert_eq!(d["database"]["url"].as_str(), Some("from-env"));
        assert_eq!(
            d["database"]["max_connections"].as_u64(),
            Some(4),
            "siblings must survive an override"
        );
    }

    #[test]
    fn overrides_create_missing_sections() {
        // A container can configure everything from the environment with no
        // config file at all.
        let mut d = Value::Mapping(serde_yaml_ng::Mapping::new());
        set_path(
            &mut d,
            &["redis".to_owned(), "url".to_owned()],
            Value::String("redis://x".to_owned()),
        );
        assert_eq!(d["redis"]["url"].as_str(), Some("redis://x"));
    }

    fn vars(pairs: &[(&str, &str)]) -> std::vec::IntoIter<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn unrelated_env_vars_are_ignored_not_rejected() {
        // OAG_ACCOUNT_SECRET is read by the admin CLI, not the config. Treating
        // it as a bad config key stopped the whole binary from booting — an
        // unrelated variable in the operator's shell was enough to break it.
        let mut d = Value::Mapping(serde_yaml_ng::Mapping::new());
        apply_overrides(
            &mut d,
            vars(&[
                ("OAG_ACCOUNT_SECRET", "some-provider-key"),
                ("OAG_CONFIG", "/etc/oag.yaml"),
                ("OAG_DATABASE__URL", "postgres://from-env/db"),
            ]),
        );
        assert_eq!(
            d["database"]["url"].as_str(),
            Some("postgres://from-env/db")
        );
        assert!(
            d.get("account_secret").is_none(),
            "an unrelated variable must not become a config key"
        );
        assert!(d.get("config").is_none());
    }

    #[test]
    fn a_bare_section_name_addresses_nothing() {
        let mut d = Value::Mapping(serde_yaml_ng::Mapping::new());
        apply_overrides(&mut d, vars(&[("OAG_SERVER", "nonsense")]));
        assert!(d.get("server").is_none());
    }

    #[test]
    fn non_prefixed_vars_are_untouched() {
        let mut d = Value::Mapping(serde_yaml_ng::Mapping::new());
        apply_overrides(&mut d, vars(&[("PATH", "/usr/bin"), ("HOME", "/root")]));
        assert!(d.as_mapping().is_some_and(serde_yaml_ng::Mapping::is_empty));
    }

    #[test]
    fn scalars_are_parsed_to_their_narrowest_type() {
        // Otherwise a numeric setting arrives as a string and fails to
        // deserialise, pointing the blame at the config file.
        assert!(parse_scalar("true").is_bool());
        assert!(parse_scalar("8080").is_number());
        assert!(parse_scalar("0.0.0.0:8080").is_string());
        assert!(
            parse_scalar("postgres://u:p@h/db").is_string(),
            "a URL is not a number"
        );
    }
}
