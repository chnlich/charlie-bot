//! Startup refusal for homes that still contain v1 session metadata.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::config::CharlieBotConfig;
use crate::metadata_slot::METADATA_FILE_NAME;

const CONVERSION_SCRIPT: &str = "scripts/v1_session_conversion.py";

/// Directions for restoring the deleted v1 session conversion script.
pub const V1_SESSION_CONVERSION_GUIDANCE: &str = "the conversion script was deleted. From the repository root, run `git show d4021ce:scripts/v1_session_conversion.py > scripts/v1_session_conversion.py` to restore it.";

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
    let path = entry.path().join(METADATA_FILE_NAME);
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
    "{} holds {count} v1 {noun}: a v1 session's metadata.json has no profile, and the server lists only sessions with a profile. Stop every writer of the home; {V1_SESSION_CONVERSION_GUIDANCE}\nAfter restoring the script, run:\n  uv run python {CONVERSION_SCRIPT} dry-run --home {}\n  uv run python {CONVERSION_SCRIPT} apply --home {}",
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

  #[test]
  fn startup_refusal_explains_how_to_restore_the_conversion_script() {
    let home = tempfile::tempdir().unwrap();
    let legacy = home.path().join("sessions/old");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("metadata.json"), r#"{"id":"old","name":"old"}"#).unwrap();
    let cfg = crate::config::load_config(home.path(), &crate::config::ConfigSchema::default()).unwrap();

    let error = require_no_v1_sessions(&cfg).unwrap_err();

    assert!(error.contains("holds 1 v1 session"), "{error}");
    assert!(error.contains("the conversion script was deleted"), "{error}");
    assert!(error.contains("scripts/v1_session_conversion.py"), "{error}");
    assert!(error.contains("git show d4021ce:scripts/v1_session_conversion.py"), "{error}");
    assert!(error.contains("uv run python scripts/v1_session_conversion.py dry-run"), "{error}");
    assert!(error.contains("uv run python scripts/v1_session_conversion.py apply"), "{error}");
  }
}
