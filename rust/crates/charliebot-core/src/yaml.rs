//! The `yaml_utils.save_yaml` writer (`src/infra/yaml_utils.py`).

use serde_json::Value;

/// Dump the JSON shaped values held by cron files with PyYAML's safe C dumper block layout.
pub fn dump(value: &Value) -> Result<String, serde_yaml::Error> {
  let mut output = String::new();
  write_value(&mut output, value, 0, None)?;
  Ok(output)
}

fn write_value(output: &mut String, value: &Value, indent: usize, key: Option<&str>) -> Result<(), serde_yaml::Error> {
  if let Some(key) = key {
    output.push_str(&" ".repeat(indent));
    output.push_str(&plain_or_quoted(key, true));
    output.push(':');
  }
  match value {
    Value::Object(map) if map.is_empty() => output.push_str(" {}\n"),
    Value::Array(items) if items.is_empty() => output.push_str(" []\n"),
    Value::Object(map) => {
      if key.is_some() {
        output.push('\n');
      }
      for (child_key, child) in map {
        write_value(output, child, indent + if key.is_some() { 2 } else { 0 }, Some(child_key))?;
      }
    }
    Value::Array(items) => {
      if key.is_some() {
        output.push('\n');
      }
      for child in items {
        output.push_str(&" ".repeat(indent));
        output.push_str("- ");
        write_sequence_item(output, child, indent + 2)?;
      }
    }
    scalar => {
      if key.is_some() {
        output.push(' ');
      }
      output.push_str(&scalar_text(scalar)?);
      output.push('\n');
    }
  }
  Ok(())
}

fn write_sequence_item(output: &mut String, value: &Value, indent: usize) -> Result<(), serde_yaml::Error> {
  match value {
    Value::Object(map) if map.is_empty() => output.push_str("{}\n"),
    Value::Array(items) if items.is_empty() => output.push_str("[]\n"),
    Value::Object(map) => {
      if let Some((first_key, first_value)) = map.iter().next() {
        output.push_str(&plain_or_quoted(first_key, true));
        output.push(':');
        match first_value {
          Value::Object(_) | Value::Array(_) => write_value(output, first_value, indent, None)?,
          scalar => {
            output.push(' ');
            output.push_str(&scalar_text(scalar)?);
            output.push('\n');
          }
        }
        for (child_key, child) in map.iter().skip(1) {
          write_value(output, child, indent, Some(child_key))?;
        }
      }
    }
    child => {
      output.push_str(&scalar_text(child)?);
      output.push('\n');
    }
  }
  Ok(())
}

fn scalar_text(value: &Value) -> Result<String, serde_yaml::Error> {
  Ok(match value {
    Value::Null => "null".to_string(),
    Value::Bool(value) => if *value { "true" } else { "false" }.to_string(),
    Value::Number(value) => value.to_string(),
    Value::String(value) => plain_or_quoted(value, false),
    Value::Array(_) | Value::Object(_) => serde_yaml::to_string(value)?.trim_end().to_string(),
  })
}

fn plain_or_quoted(value: &str, key: bool) -> String {
  if plain_safe(value, key) {
    return value.to_string();
  }
  if value.contains('\n') && !value.chars().any(|ch| ch.is_control() && ch != '\n') {
    let mut quoted = String::from("'");
    for (index, line) in value.split('\n').enumerate() {
      if index > 0 {
        if line.is_empty() {
          quoted.push('\n');
        } else {
          quoted.push_str("\n\n  ");
        }
      }
      quoted.push_str(&line.replace('\'', "''"));
    }
    quoted.push('\'');
    return quoted;
  }
  if !value.chars().any(char::is_control) {
    return format!("'{}'", value.replace('\'', "''"));
  }
  serde_json::to_string(value).expect("a string serializes as JSON")
}

fn plain_safe(value: &str, key: bool) -> bool {
  if value.is_empty() || value.trim() != value || value.contains(['\n', '\r', '\t']) {
    return false;
  }
  if value.starts_with(['-', '?', ':'])
    || value.starts_with(['[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
  {
    return false;
  }
  if value.contains(": ") || (!key && value.contains(" #")) || value.ends_with(':') || value.ends_with(' ') {
    return false;
  }
  if yaml_ambiguous_scalar(value) {
    return false;
  }
  true
}

fn yaml_ambiguous_scalar(value: &str) -> bool {
  let lowered = value.to_ascii_lowercase();
  if matches!(
    lowered.as_str(),
    "null" | "~" | "true" | "false" | "yes" | "no" | "on" | "off" | ".inf" | "+.inf" | "-.inf" | ".nan"
  ) {
    return true;
  }
  if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok() {
    return true;
  }
  let bytes = value.as_bytes();
  bytes.len() >= 8
    && bytes[0].is_ascii_digit()
    && bytes[1..].contains(&b'-')
    && value.chars().all(|ch| ch.is_ascii_digit() || matches!(ch, '-' | ':' | '.' | 'T' | 't' | 'Z' | 'z' | '+'))
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::dump;

  #[test]
  fn dump_keeps_cron_yaml_mapping_order_and_pyyaml_string_quoting() {
    assert_eq!(
      dump(&json!({"name": "daily", "cron": "0 8 * * *", "enabled": true, "prompt": "line one\n\nline two"})).unwrap(),
      "name: daily\ncron: 0 8 * * *\nenabled: true\nprompt: 'line one\n\n\n  line two'\n"
    );
    assert_eq!(dump(&json!({"value": "on"})).unwrap(), "value: 'on'\n");
    assert_eq!(dump(&json!({"value": "雪と café"})).unwrap(), "value: 雪と café\n");
  }

  #[test]
  fn malformed_yaml_remains_an_error() {
    // Python: tests/test_yaml_utils.py::test_malformed_document_raises_yaml_error
    assert!(serde_yaml::from_str::<serde_json::Value>("a: [1, 2\nb: {c").is_err());
  }
}
