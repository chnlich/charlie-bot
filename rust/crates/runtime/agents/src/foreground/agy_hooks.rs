//! The user-level agy hook file that carries the foreground cap. The agy backend ensures its entry before each launch:
//! the merge keeps every other key, and the file is replaced atomically.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::{AGY_HOOK_KEY, HOOK_COMMAND};

/// The agy tool whose `CommandLine` the cap rewrites.
const AGY_SHELL_TOOL_MATCHER: &str = "^run_command$";
/// The timeout of the hook command, in seconds, as the other agy hooks carry it.
const HOOK_TIMEOUT_SECS: u64 = 10;

/// The user-level hook file that agy reads: `~/.gemini/config/hooks.json`.
pub fn agy_hooks_path() -> io::Result<PathBuf> {
  let home = std::env::var_os("HOME")
    .filter(|home| !home.is_empty())
    .ok_or_else(|| io::Error::other("HOME is unset, so the agy hook file cannot be located"))?;
  Ok(PathBuf::from(home).join(".gemini/config/hooks.json"))
}

/// The entry that the cap's agy hook carries.
fn cap_entry() -> Value {
  json!({
    "PreToolUse": [{
      "matcher": AGY_SHELL_TOOL_MATCHER,
      "hooks": [{"type": "command", "command": HOOK_COMMAND, "timeout": HOOK_TIMEOUT_SECS}]
    }]
  })
}

/// Ensure the cap entry is in the hook file at `path`, merging it with the file's other keys. Return whether the file
/// changed. A missing file starts empty, and a file that is not a JSON object is an error.
pub fn ensure_cap_hook(path: &Path) -> io::Result<bool> {
  let mut root = match fs::read_to_string(path) {
    Ok(text) => serde_json::from_str::<Value>(&text)
      .map_err(|error| io::Error::other(format!("agy hook file {} is not JSON: {error}", path.display())))?,
    Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
    Err(error) => return Err(error),
  };
  let entries = root
    .as_object_mut()
    .ok_or_else(|| io::Error::other(format!("agy hook file {} is not a JSON object", path.display())))?;
  let desired = cap_entry();
  if entries.get(AGY_HOOK_KEY) == Some(&desired) {
    return Ok(false);
  }
  entries.insert(AGY_HOOK_KEY.to_string(), desired);
  write_atomically(path, &root)?;
  Ok(true)
}

/// Write `value` to `path` through a temporary file in the same directory, renamed over `path`.
fn write_atomically(path: &Path, value: &Value) -> io::Result<()> {
  let parent = path.parent().ok_or_else(|| io::Error::other(format!("{} has no directory", path.display())))?;
  fs::create_dir_all(parent)?;
  let text = serde_json::to_string_pretty(value).map_err(io::Error::other)? + "\n";
  let mut id = [0u8; 8];
  getrandom::fill(&mut id).map_err(io::Error::other)?;
  let id = id.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
  let temporary = parent.join(format!(
    ".{}.tmp-{}-{id}",
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
    std::process::id()
  ));
  let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
  file.write_all(text.as_bytes())?;
  file.sync_all()?;
  drop(file);
  fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn hook_file(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("hooks.json");
    fs::write(&path, text).unwrap();
    path
  }

  #[test]
  fn a_missing_file_gets_the_cap_entry_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config/hooks.json");
    assert!(ensure_cap_hook(&path).unwrap());
    let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written, json!({AGY_HOOK_KEY: cap_entry()}));
  }

  #[test]
  fn the_merge_keeps_the_schedule_guard_and_any_other_key() {
    let dir = tempfile::tempdir().unwrap();
    let guard = json!({"PreToolUse": [{"matcher": "^schedule$", "hooks": [{"type": "command", "command": "/x/deny_schedule.py"}]}]});
    let path = hook_file(dir.path(), &json!({"headless-schedule-guard": guard, "other": {"keep": true}}).to_string());
    assert!(ensure_cap_hook(&path).unwrap());
    let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written["headless-schedule-guard"], guard);
    assert_eq!(written["other"], json!({"keep": true}));
    assert_eq!(written[AGY_HOOK_KEY], cap_entry());
  }

  #[test]
  fn a_second_ensure_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = hook_file(dir.path(), "{}");
    assert!(ensure_cap_hook(&path).unwrap());
    let first = fs::read_to_string(&path).unwrap();
    assert!(!ensure_cap_hook(&path).unwrap());
    assert_eq!(fs::read_to_string(&path).unwrap(), first);
  }

  #[test]
  fn a_file_that_is_not_a_json_object_is_an_error_and_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = hook_file(dir.path(), "[1, 2]");
    let error = ensure_cap_hook(&path).unwrap_err();
    assert!(error.to_string().contains("not a JSON object"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "[1, 2]");
  }

  #[test]
  fn no_temporary_file_is_left_after_the_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = hook_file(dir.path(), "{}");
    ensure_cap_hook(&path).unwrap();
    let names: Vec<String> = fs::read_dir(dir.path())
      .unwrap()
      .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
      .collect();
    assert_eq!(names, vec!["hooks.json".to_string()]);
  }

  #[test]
  fn concurrent_launches_can_ensure_the_same_hook_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = hook_file(dir.path(), "{}");
    let workers: Vec<_> = (0..16)
      .map(|_| {
        let path = path.clone();
        std::thread::spawn(move || ensure_cap_hook(&path).unwrap())
      })
      .collect();
    for worker in workers {
      worker.join().unwrap();
    }
    let written: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(written[AGY_HOOK_KEY], cap_entry());
  }
}
