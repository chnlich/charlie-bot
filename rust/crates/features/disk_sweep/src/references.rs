//! The external reference check: the records `disk_sweep.reference_command` prints, and the match of a candidate path
//! against them.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use charliebot_core::paths;
use serde_json::Value;

/// Runs the reference command once and returns its records. Any failure is an error for the whole sweep.
pub(crate) fn load(command: &str) -> Result<Vec<Value>> {
  let output = Command::new("sh")
    .arg("-c")
    .arg(command)
    .output()
    .with_context(|| format!("running disk_sweep.reference_command {command:?}"))?;
  if !output.status.success() {
    bail!(
      "disk_sweep.reference_command {command:?} exited with {}: {}",
      output.status,
      String::from_utf8_lossy(&output.stderr).trim()
    );
  }
  let value: Value = serde_json::from_slice(&output.stdout)
    .with_context(|| format!("disk_sweep.reference_command {command:?} did not print JSON"))?;
  match value {
    Value::Array(records) => Ok(records),
    _ => bail!("disk_sweep.reference_command {command:?} must print a JSON array"),
  }
}

/// Whether any string value in `records` names `path`, written as `~/…`, as its absolute path, or as its real path.
pub(crate) fn names(records: &[Value], path: &Path) -> bool {
  let forms = path_forms(path);
  records.iter().any(|record| value_names(record, &forms))
}

fn value_names(value: &Value, forms: &[String]) -> bool {
  match value {
    Value::String(text) => forms.iter().any(|form| mentions(text, form)),
    Value::Array(items) => items.iter().any(|item| value_names(item, forms)),
    Value::Object(fields) => fields.values().any(|field| value_names(field, forms)),
    Value::Null | Value::Bool(_) | Value::Number(_) => false,
  }
}

/// Whether `form` occurs in `text` followed by the end of the text or by a character that cannot continue a path
/// component. `wt-1` therefore does not match inside `wt-10`.
fn mentions(text: &str, form: &str) -> bool {
  text.match_indices(form).any(|(index, _)| {
    text[index + form.len()..].chars().next().is_none_or(|next| !(next.is_alphanumeric() || next == '-' || next == '_'))
  })
}

fn path_forms(path: &Path) -> Vec<String> {
  let absolute = path.to_string_lossy().into_owned();
  let real = paths::realpath(path).to_string_lossy().into_owned();
  let home = paths::expanduser("~");
  let mut forms = Vec::new();
  for form in [absolute.clone(), real.clone()] {
    if let Some(rest) =
      form.strip_prefix(home.trim_end_matches('/')).filter(|rest| rest.is_empty() || rest.starts_with('/'))
    {
      forms.push(format!("~{rest}"));
    }
  }
  forms.push(absolute);
  forms.push(real);
  forms.sort();
  forms.dedup();
  forms
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use serde_json::json;

  use super::names;

  #[test]
  fn test_home_relative_record_names_a_path_only_as_whole_components() {
    let home = std::env::var("HOME").expect("the test runs with HOME set");
    let candidate = PathBuf::from(home).join("scripts/zz-candidate");
    let whole = [json!({"remote_workdir": "~/scripts/zz-candidate/repo-submit"})];
    let sibling = [json!({"remote_workdir": "~/scripts/zz-candidate2"})];
    assert!(names(&whole, &candidate));
    assert!(!names(&sibling, &candidate));
  }
}
