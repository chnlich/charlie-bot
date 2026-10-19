//! The antigravity option fields (`src/backends/antigravity/options.py`): `print_timeout`, the `agy --print` turn budget.

use serde_yaml::{Mapping, Value};

const PRINT_TIMEOUT: &str = "print_timeout";

/// The `print_timeout` the option sets, or `None` when it sets none. Any other key is refused.
pub fn print_timeout(fields: &Mapping) -> Result<Option<String>, String> {
  for key in fields.keys() {
    match key.as_str() {
      Some(PRINT_TIMEOUT) => {}
      Some(name) => return Err(format!("unknown antigravity option field {name:?}")),
      None => return Err(format!("antigravity option key must be a string: {key:?}")),
    }
  }
  match fields.get(PRINT_TIMEOUT) {
    None | Some(Value::Null) => Ok(None),
    Some(Value::String(timeout)) => Ok(Some(timeout.clone())),
    Some(_) => Err("antigravity option print_timeout must be a string or null".to_string()),
  }
}

/// The option model the registry runs over each `antigravity` entry at config load.
pub fn option_model(fields: &Mapping) -> Result<(), String> {
  print_timeout(fields).map(|_| ())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn print_timeout_is_an_optional_text_and_no_other_field_is_accepted() {
    let fields = |text: &str| -> Mapping { serde_yaml::from_str(text).unwrap() };
    assert_eq!(print_timeout(&fields("print_timeout: 2h\n")).unwrap().as_deref(), Some("2h"));
    assert_eq!(print_timeout(&fields("print_timeout: null\n")).unwrap(), None);
    assert!(print_timeout(&fields("effort: high\n")).is_err());
  }
}
