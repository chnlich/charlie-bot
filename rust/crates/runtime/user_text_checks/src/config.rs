//! The `user_text_checks` config section: its patterns compile when the config loads and dump back as source text.

use charliebot_core::config::SectionValue;
use fancy_regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};
use serde_yaml::Value as YamlValue;

#[derive(Clone)]
pub(crate) struct InternalIdPattern {
  source: String,
  pub(crate) compiled: Regex,
}

/// The compiled user_text_checks.internal_id_patterns config section.
#[derive(Clone, Default)]
pub struct UserTextChecksConfig {
  pub(crate) patterns: Vec<InternalIdPattern>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUserTextChecksConfig {
  #[serde(default)]
  internal_id_patterns: Vec<String>,
}

pub(crate) fn parse_config(value: &YamlValue) -> Result<SectionValue, String> {
  let raw: RawUserTextChecksConfig =
    serde_yaml::from_value(value.clone()).map_err(|error| format!("user_text_checks: {error}"))?;
  let patterns = raw
    .internal_id_patterns
    .into_iter()
    .map(|source| {
      Regex::new(&source)
        .map(|compiled| InternalIdPattern { source: source.clone(), compiled })
        .map_err(|error| format!("internal_id_patterns pattern {source:?} is invalid: {error}"))
    })
    .collect::<Result<Vec<_>, _>>()?;
  Ok(std::sync::Arc::new(UserTextChecksConfig { patterns }))
}

pub(crate) fn dump_config(value: &SectionValue) -> Result<Value, String> {
  let config = value.downcast_ref::<UserTextChecksConfig>().ok_or("user_text_checks holds an unexpected value")?;
  let patterns = config.patterns.iter().map(|pattern| Value::String(pattern.source.clone())).collect();
  let mut object = Map::new();
  object.insert("internal_id_patterns".to_string(), Value::Array(patterns));
  Ok(Value::Object(object))
}
