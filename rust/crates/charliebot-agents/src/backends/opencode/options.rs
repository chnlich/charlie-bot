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

/// The config keys that the options' `context_limit` values add, as Python's `model_overrides_config` builds them:
/// `{"providers": {provider: {"models": {model: {"limit": {"context": n}}}}}}`, or an empty map when no option sets a
/// limit. Two options that name one model must declare one limit, or the build fails with both option ids.
pub fn model_overrides_config(options: &[BackendOption]) -> anyhow::Result<Map<String, JsonValue>> {
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

  const MODEL: &str = "fakeprov/fake-model";

  fn fields(text: &str) -> Mapping {
    serde_yaml::from_str(text).unwrap()
  }

  fn option(id: &str, model: &str, type_fields: &str) -> BackendOption {
    BackendOption {
      id: id.to_string(),
      label: id.to_string(),
      model: model.to_string(),
      prompt_overlay: None,
      backend_type: "opencode".to_string(),
      type_fields: fields(type_fields),
    }
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

  // Python: tests/test_opencode_backend.py::test_model_overrides_config_overrides_only_context
  #[test]
  fn model_overrides_config_overrides_only_context() {
    let other = BackendOption {
      id: "other".to_string(),
      label: "Other".to_string(),
      model: MODEL.to_string(),
      prompt_overlay: None,
      backend_type: "codex".to_string(),
      type_fields: Mapping::new(),
    };
    assert_eq!(model_overrides_config(&[other.clone(), option("oc", "provider/org/model", "{}")]).unwrap(), Map::new());
    assert_eq!(
      JsonValue::Object(
        model_overrides_config(&[other, option("oc", "provider/org/model", "context_limit: 420000\n")]).unwrap()
      ),
      json!({"providers": {"provider": {"models": {"org/model": {"limit": {"context": 420000}}}}}})
    );
    assert_eq!(model_overrides_config(&[]).unwrap(), Map::new());
  }

  // Python: tests/test_opencode_backend.py::test_model_overrides_config_rejects_conflicts_naming_both_options
  #[test]
  fn model_overrides_config_rejects_conflicts_naming_both_options() {
    for (first_fields, second_fields, field) in [
      ("context_limit: 420000\n", "context_limit: 120000\n", "context_limit"),
      ("extra_body:\n  cache: true\n", "extra_body:\n  cache: false\n", "extra_body"),
    ] {
      let first = option("oc-first", MODEL, first_fields);
      let second = option("oc-second", MODEL, second_fields);
      let error = model_overrides_config(&[first, second]).unwrap_err().to_string();
      assert!(error.contains("oc-first"), "{error}");
      assert!(error.contains("oc-second"), "{error}");
      assert!(error.contains(field), "{error}");
    }
  }

  // Python: tests/test_opencode_backend.py::test_model_overrides_config_carries_extra_body_at_the_model_body_key
  #[test]
  fn model_overrides_config_carries_extra_body_at_the_model_body_key() {
    let option = option("oc", MODEL, "extra_body:\n  cache:\n    no-store: true\n    no-cache: true\n");
    let config = model_overrides_config(&[option]).unwrap();
    assert_eq!(
      config["providers"]["fakeprov"]["models"]["fake-model"]["body"],
      json!({"cache": {"no-store": true, "no-cache": true}})
    );
  }

  // Python: tests/test_opencode_backend.py::test_model_overrides_config_keeps_context_and_extra_body_of_two_options_on_one_model
  #[test]
  fn model_overrides_config_keeps_context_and_extra_body_of_two_options_on_one_model() {
    let limited = option("limited", MODEL, "context_limit: 420000\n");
    let cached = option("cached", MODEL, "extra_body:\n  cache:\n    no-store: true\n    no-cache: true\n");
    let config = model_overrides_config(&[limited, cached]).unwrap();
    assert_eq!(
      config["providers"]["fakeprov"]["models"]["fake-model"],
      json!({"limit": {"context": 420000}, "body": {"cache": {"no-store": true, "no-cache": true}}})
    );
  }

  // Python: tests/test_opencode_backend.py::test_model_overrides_config_merges_equal_limits_and_distinct_models
  #[test]
  fn model_overrides_config_merges_equal_limits_and_distinct_models() {
    let entries = [
      ("first", "provider/model", "context_limit: 420000\n"),
      ("equal", "provider/model", "context_limit: 420000\n"),
      ("uncapped", "provider/model", "{}"),
      ("sibling", "provider/other", "context_limit: 120000\n"),
      ("other", "other/model", "context_limit: 200000\n"),
    ]
    .map(|(id, model, type_fields)| option(id, model, type_fields));
    assert_eq!(
      JsonValue::Object(model_overrides_config(&entries).unwrap()),
      json!({
        "providers": {
          "provider": {"models": {"model": {"limit": {"context": 420000}}, "other": {"limit": {"context": 120000}}}},
          "other": {"models": {"model": {"limit": {"context": 200000}}}},
        }
      })
    );
  }
}
