//! Startup refusal for homes that still contain v1 session metadata.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::config::CharlieBotConfig;

const CONVERSION_SCRIPT: &str = "scripts/v1_session_conversion.py";

/// Count session directories whose metadata has no non-null `profile` field.
pub fn count_v1_sessions(sessions_dir: &Path) -> Result<usize, String> {
  if !sessions_dir.is_dir() {
    return Ok(0);
  }
  let entries = fs::read_dir(sessions_dir).map_err(|error| format!("{}: {error}", sessions_dir.display()))?;
  let mut count = 0;
  for entry in entries {
    let entry = entry.map_err(|error| format!("{}: {error}", sessions_dir.display()))?;
    let name = entry.file_name();
    let name = name.to_string_lossy();
    if !entry.path().is_dir() || (name.starts_with(".task-") && name.ends_with(".tmp")) {
      continue;
    }
    let path = entry.path().join("metadata.json");
    let raw = match fs::read(&path) {
      Ok(raw) => raw,
      Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
      Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let metadata: Value =
      serde_json::from_slice(&raw).map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
    let object = metadata.as_object().ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
    if object.get("profile").is_none_or(Value::is_null) {
      count += 1;
    }
  }
  Ok(count)
}

/// Refuse startup when the home contains sessions that would disappear from v2 listings.
pub fn require_no_v1_sessions(cfg: &CharlieBotConfig) -> Result<(), String> {
  let count = count_v1_sessions(&cfg.sessions_dir())?;
  if count == 0 {
    return Ok(());
  }
  let home = &cfg.charliebot_home;
  let noun = if count == 1 { "session" } else { "sessions" };
  Err(format!(
    "{} holds {count} v1 {noun}: a v1 session's metadata.json has no profile, and the server lists only sessions with a profile. Stop every writer of the home, then convert it:\n  uv run python {CONVERSION_SCRIPT} dry-run --home {}\n  uv run python {CONVERSION_SCRIPT} apply --home {}",
    home.display(),
    home.display(),
    home.display()
  ))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn counts_only_session_directories_without_profiles() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions");
    fs::create_dir_all(sessions.join("old")).unwrap();
    fs::write(sessions.join("old/metadata.json"), r#"{"id":"old"}"#).unwrap();
    fs::create_dir_all(sessions.join("new")).unwrap();
    fs::write(sessions.join("new/metadata.json"), r#"{"profile":"manager"}"#).unwrap();
    fs::create_dir_all(sessions.join(".task-pending.tmp")).unwrap();
    fs::write(sessions.join(".task-pending.tmp/metadata.json"), "{}").unwrap();
    assert_eq!(count_v1_sessions(&sessions).unwrap(), 1);
  }
}
