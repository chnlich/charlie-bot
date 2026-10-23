//! The opencode option fields and the `providers` block their context limits add to an opencode config
//! (`src/backends/opencode/options.py`).

use std::collections::HashMap;

use anyhow::bail;
use charliebot_core::config::BackendOption;
use serde_json::{Map, Value as JsonValue, json};
use serde_yaml::{Mapping, Value};

const PROXY_URL: &str = "proxy_url";
const CONTEXT_LIMIT: &str = "context_limit";

/// The type-specific fields of one opencode backend option.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenCodeOption {
  /// The proxy used by the shared server and its turn shells.
  pub proxy_url: Option<String>,
  /// The context window in tokens that opencode uses for the option's model, instead of the catalog's window.
  pub context_limit: Option<u64>,
}

impl OpenCodeOption {
  pub fn from_fields(fields: &Mapping) -> Result<Self, String> {
    for key in fields.keys() {
      match key.as_str() {
        Some(PROXY_URL | CONTEXT_LIMIT) => {}
        Some(name) => return Err(format!("opencode option field {name:?} is not supported")),
        None => return Err(format!("opencode option key must be a string: {key:?}")),
      }
    }
    let proxy_url = match fields.get(PROXY_URL) {
      None | Some(Value::Null) => None,
      Some(Value::String(value)) => Some(value.clone()),
      Some(_) => return Err("opencode option proxy_url must be a string or null".to_string()),
    };
    let context_limit = match fields.get(CONTEXT_LIMIT) {
      None | Some(Value::Null) => None,
      Some(Value::Number(value)) => match value.as_u64() {
        Some(limit) if limit > 0 => Some(limit),
        _ => return Err("opencode option context_limit must be an integer greater than 0".to_string()),
      },
      Some(_) => return Err("opencode option context_limit must be an integer greater than 0".to_string()),
    };
    Ok(Self { proxy_url, context_limit })
  }
}

/// The option model the registry runs over each opencode entry at config load.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  OpenCodeOption::from_fields(fields).map(|_| ())
}

/// The config keys that the options' `context_limit` values add, as Python's `model_context_config` builds them:
/// `{"providers": {provider: {"models": {model: {"limit": {"context": n}}}}}}`, or an empty map when no option sets a
/// limit. Two options that name one model must declare one limit, or the build fails with both option ids.
pub fn model_context_config(options: &[BackendOption]) -> anyhow::Result<Map<String, JsonValue>> {
  let mut providers = Map::new();
  let mut declared: HashMap<(String, String), (&str, u64)> = HashMap::new();
  for option in options.iter().filter(|option| option.backend_type == "opencode") {
    let Some(limit) = OpenCodeOption::from_fields(&option.type_fields).map_err(anyhow::Error::msg)?.context_limit
    else {
      continue;
    };
    let Some((provider, model)) = option.model.split_once('/') else {
      bail!("backend '{}' model {:?} must name a provider and a model", option.id, option.model);
    };
    let key = (provider.to_string(), model.to_string());
    if let Some((previous_id, previous_limit)) = declared.get(&key)
      && *previous_limit != limit
    {
      bail!(
        "opencode options '{previous_id}' and '{}' declare conflicting context_limit values for {provider}/{model}: \
         {previous_limit} and {limit}",
        option.id
      );
    }
    declared.insert(key, (&option.id, limit));
    let provider_entry = providers.entry(provider.to_string()).or_insert_with(|| json!({"models": {}}));
    provider_entry["models"][model] = json!({"limit": {"context": limit}});
  }
  let mut config = Map::new();
  if !providers.is_empty() {
    config.insert("providers".to_string(), JsonValue::Object(providers));
  }
  Ok(config)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(text: &str) -> Mapping {
    serde_yaml::from_str(text).unwrap()
  }

  #[test]
  fn option_model_accepts_proxy_url_and_rejects_other_fields() {
    assert_eq!(
      OpenCodeOption::from_fields(&fields("proxy_url: http://proxy.test:8080\n")).unwrap().proxy_url,
      Some("http://proxy.test:8080".to_string())
    );
    assert_eq!(OpenCodeOption::from_fields(&fields("proxy_url: null\n")).unwrap(), OpenCodeOption::default());
    assert!(OpenCodeOption::from_fields(&fields("other: value\n")).unwrap_err().contains("other"));
    assert!(OpenCodeOption::from_fields(&fields("proxy_url: 4\n")).unwrap_err().contains("proxy_url"));
  }

  // Python: tests/test_opencode_backend.py::test_opencode_option_rejects_invalid_fields
  #[test]
  fn option_model_accepts_a_positive_context_limit_and_rejects_zero_and_negative() {
    assert!(OpenCodeOption::from_fields(&fields("context_limit: 420000\n")).is_ok());
    assert!(OpenCodeOption::from_fields(&fields("context_limit: 0\n")).is_err());
    assert!(OpenCodeOption::from_fields(&fields("context_limit: -1\n")).is_err());
  }
}
