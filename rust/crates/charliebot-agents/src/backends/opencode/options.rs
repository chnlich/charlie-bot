//! The opencode option fields (`src/backends/opencode/options.py`).

use serde_yaml::{Mapping, Value};

const PROXY_URL: &str = "proxy_url";

/// The type-specific fields of one opencode backend option.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenCodeOption {
  /// The proxy used by the shared server and its turn shells.
  pub proxy_url: Option<String>,
}

impl OpenCodeOption {
  pub fn from_fields(fields: &Mapping) -> Result<Self, String> {
    for key in fields.keys() {
      match key.as_str() {
        Some(PROXY_URL) => {}
        Some(name) => return Err(format!("opencode option field {name:?} is not supported")),
        None => return Err(format!("opencode option key must be a string: {key:?}")),
      }
    }
    let proxy_url = match fields.get(PROXY_URL) {
      None | Some(Value::Null) => None,
      Some(Value::String(value)) => Some(value.clone()),
      Some(_) => return Err("opencode option proxy_url must be a string or null".to_string()),
    };
    Ok(Self { proxy_url })
  }
}

/// The option model the registry runs over each opencode entry at config load.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  OpenCodeOption::from_fields(fields).map(|_| ())
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
}
