//! The per-launch Devin config that carries the foreground cap. `--config` replaces the user's config file, so the written
//! file holds the user's settings with the cap's `PreToolUse` entry for the `exec` tool added.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::hook::pre_tool_use_entry;

/// The Devin tool that runs a shell command.
const DEVIN_EXEC_MATCHER: &str = "exec";
const RUNS_DIR_NAME: &str = "runs";
const DATA_DIR_NAME: &str = "data";

/// The user's Devin config file: `$XDG_CONFIG_HOME/devin/config.json`, or `~/.config/devin/config.json` when that is unset.
pub fn user_devin_config_path() -> io::Result<PathBuf> {
  let config_home = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
    Some(config_home) => PathBuf::from(config_home),
    None => {
      let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| io::Error::other("HOME is unset, so the Devin config file cannot be located"))?;
      PathBuf::from(home).join(".config")
    }
  };
  Ok(config_home.join("devin/config.json"))
}

/// The Devin config path in the CharlieBot session directory that owns `run_dir`.
pub fn session_devin_config_path(run_dir: &Path) -> io::Result<PathBuf> {
  let runs_dir = run_dir
    .parent()
    .filter(|path| path.file_name().is_some_and(|name| name == RUNS_DIR_NAME))
    .ok_or_else(|| io::Error::other(format!("Devin run directory {} is not under data/runs", run_dir.display())))?;
  let data_dir = runs_dir
    .parent()
    .filter(|path| path.file_name().is_some_and(|name| name == DATA_DIR_NAME))
    .ok_or_else(|| io::Error::other(format!("Devin run directory {} is not under data/runs", run_dir.display())))?;
  let session_dir = data_dir
    .parent()
    .ok_or_else(|| io::Error::other(format!("Devin run directory {} has no session directory", run_dir.display())))?;
  Ok(session_dir.join("devin_config.json"))
}

/// Write the per-launch config to `target`: the user config at `user_config` (an absent file is an empty config, as Devin
/// reads it) with the cap's `exec` entry appended to its `PreToolUse` hooks.
pub fn write_devin_config(target: &Path, user_config: &Path) -> io::Result<()> {
  let mut root = match fs::read_to_string(user_config) {
    Ok(text) => serde_json::from_str::<Value>(&text)
      .map_err(|error| io::Error::other(format!("Devin config {} is not JSON: {error}", user_config.display())))?,
    Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
    Err(error) => return Err(error),
  };
  let config = root
    .as_object_mut()
    .ok_or_else(|| io::Error::other(format!("Devin config {} is not a JSON object", user_config.display())))?;
  let hooks = config.entry("hooks").or_insert_with(|| json!({}));
  let hooks = hooks.as_object_mut().ok_or_else(|| {
    io::Error::other(format!("the hooks of Devin config {} are not an object", user_config.display()))
  })?;
  let pre_tool_use = hooks.entry("PreToolUse").or_insert_with(|| json!([]));
  pre_tool_use
    .as_array_mut()
    .ok_or_else(|| {
      io::Error::other(format!("the PreToolUse hooks of Devin config {} are not an array", user_config.display()))
    })?
    .push(pre_tool_use_entry(DEVIN_EXEC_MATCHER));
  let text = serde_json::to_string_pretty(&root).map_err(io::Error::other)? + "\n";
  fs::write(target, text)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_absent_user_config_becomes_a_config_with_the_exec_hook_alone() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("devin_config.json");
    write_devin_config(&target, &dir.path().join("missing/config.json")).unwrap();
    let written: Value = serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(written, json!({"hooks": {"PreToolUse": [pre_tool_use_entry("exec")]}}));
  }

  #[test]
  fn the_user_settings_and_their_hooks_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("config.json");
    let existing = json!({"model": "m", "hooks": {"PreToolUse": [{"matcher": "read", "hooks": []}], "Stop": []}});
    fs::write(&user, existing.to_string()).unwrap();
    let target = dir.path().join("devin_config.json");
    write_devin_config(&target, &user).unwrap();
    let written: Value = serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(written["model"], json!("m"));
    assert_eq!(written["hooks"]["Stop"], json!([]));
    assert_eq!(written["hooks"]["PreToolUse"][0]["matcher"], json!("read"));
    assert_eq!(written["hooks"]["PreToolUse"][1], pre_tool_use_entry("exec"));
  }

  #[test]
  fn a_user_config_that_is_not_json_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("config.json");
    fs::write(&user, "not json").unwrap();
    let error = write_devin_config(&dir.path().join("out.json"), &user).unwrap_err();
    assert!(error.to_string().contains("is not JSON"));
  }

  #[test]
  fn the_per_launch_config_lives_in_its_session_directory() {
    let run_dir = Path::new("/profile/sessions/session-1/data/runs/run-1");
    assert_eq!(session_devin_config_path(run_dir).unwrap(), Path::new("/profile/sessions/session-1/devin_config.json"));
  }

  #[test]
  fn a_run_directory_outside_the_session_store_has_no_config_path() {
    let error = session_devin_config_path(Path::new("/tmp/charliebot-run-1")).unwrap_err();
    assert!(error.to_string().contains("not under data/runs"));
  }
}
