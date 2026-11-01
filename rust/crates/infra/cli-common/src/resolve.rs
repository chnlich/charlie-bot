//! Session resolution: the server-written variable, the explicit argument, or the working directory.

use std::path::Path;

use charliebot_core::config;
use charliebot_core::run_token::SESSION_ID_ENV;
use serde_json::{Value, json};

use crate::exit::CliExit;
use crate::json_out;

/// The resolution inputs, passed in so tests exercise the rules without touching process state.
pub struct SessionSources<'a> {
  /// The `--session` value.
  pub arg_session: Option<&'a str>,
  /// The server-written variable's value, `None` when unset or empty.
  pub env_session: Option<&'a str>,
  /// The process working directory, resolved.
  pub cwd: &'a Path,
  /// The profile home's `sessions` directory, resolved.
  pub sessions_dir: &'a Path,
}

/// What `resolve_session_id` decided: the session id and the non-fatal note to print on stderr.
#[derive(Debug, PartialEq)]
pub struct Resolved {
  pub session_id: String,
  pub note: Option<String>,
}

/// `resolve_session_id`: the server-written variable wins; an explicit `--session` must agree with
/// it and a mismatch exits 2 naming both. Without the variable, `--session` or a cwd whose parent
/// is the sessions directory supplies the id — both given, they must agree; neither, exit 2.
pub fn resolve_session_id(sources: SessionSources<'_>) -> Result<Resolved, CliExit> {
  let cwd_session = cwd_session(sources.cwd, sources.sessions_dir);

  // An empty value carries no identity, so it reads as absent and the cwd fallback answers.
  if let Some(env_session) = sources.env_session {
    if let Some(arg_session) = sources.arg_session
      && arg_session != env_session
    {
      return Err(ambiguous(format!("--session={arg_session} {SESSION_ID_ENV}={env_session}")));
    }
    let mut note = None;
    if let Some(cwd_session) = &cwd_session
      && cwd_session != env_session
    {
      note = Some(format!("cwd is session dir of {cwd_session}; using {SESSION_ID_ENV}={env_session}"));
    }
    return Ok(Resolved { session_id: env_session.to_string(), note });
  }

  let mut sources_in_order: Vec<(&str, &str)> = Vec::new();
  if let Some(arg_session) = sources.arg_session {
    sources_in_order.push(("--session", arg_session));
  }
  if let Some(cwd_session) = &cwd_session {
    sources_in_order.push(("cwd", cwd_session));
  }
  if sources_in_order.is_empty() {
    return Err(CliExit::usage("--session required when not running from a CharlieBot session dir"));
  }
  let mut ids: Vec<&str> = Vec::new();
  for (_, value) in &sources_in_order {
    if !ids.contains(value) {
      ids.push(value);
    }
  }
  if ids.len() > 1 {
    let named = sources_in_order.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join(" ");
    return Err(ambiguous(named));
  }
  Ok(Resolved { session_id: ids[0].to_string(), note: None })
}

/// The process-level entry: reads the environment and cwd, then prints the non-fatal note.
pub fn resolve_from_env(arg_session: Option<&str>) -> Result<String, CliExit> {
  let home = charliebot_core::home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let sessions_dir = config::sessions_dir_in(&home);
  let cwd = std::env::current_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let env_session = std::env::var(SESSION_ID_ENV).ok().filter(|value| !value.is_empty());
  let resolved = resolve_session_id(SessionSources {
    arg_session,
    env_session: env_session.as_deref(),
    cwd: &cwd,
    sessions_dir: &sessions_dir,
  })?;
  if let Some(note) = resolved.note {
    let note_json: Value = json!({ "note": note });
    eprint!("{}", json_out::render(&note_json, None));
  }
  Ok(resolved.session_id)
}

/// The session id the cwd names: its file name when the cwd's parent is the sessions directory.
fn cwd_session(cwd: &Path, sessions_dir: &Path) -> Option<String> {
  if cwd.parent() == Some(sessions_dir) {
    cwd.file_name().map(|name| name.to_string_lossy().into_owned())
  } else {
    None
  }
}

fn ambiguous(source_text: String) -> CliExit {
  CliExit::usage(format!("session id mismatch: {source_text}; refusing to use an ambiguous session"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  struct Layout {
    root: tempfile::TempDir,
    sessions_dir: PathBuf,
  }

  impl Layout {
    fn new() -> Layout {
      let root = tempfile::tempdir().unwrap();
      let sessions_dir = root.path().join("sessions");
      std::fs::create_dir(&sessions_dir).unwrap();
      Layout { root, sessions_dir }
    }

    fn session_dir(&self, name: &str) -> PathBuf {
      let dir = self.sessions_dir.join(name);
      std::fs::create_dir_all(&dir).unwrap();
      dir
    }

    fn outside(&self) -> PathBuf {
      let dir = self.root.path().join("outside");
      std::fs::create_dir(&dir).unwrap();
      dir
    }
  }

  fn resolve(layout: &Layout, cwd: &Path, arg: Option<&str>, env: Option<&str>) -> Result<Resolved, CliExit> {
    resolve_session_id(SessionSources { arg_session: arg, env_session: env, cwd, sessions_dir: &layout.sessions_dir })
  }

  #[test]
  fn test_resolve_session_id_sources_without_env() {
    let layout = Layout::new();
    let explicit = layout.outside();
    assert_eq!(resolve(&layout, &explicit, Some("explicit"), None).unwrap().session_id, "explicit");
    let cwd_dir = layout.session_dir("cwd-session");
    assert_eq!(resolve(&layout, &cwd_dir, None, None).unwrap().session_id, "cwd-session");
    let both = layout.session_dir("same-session");
    assert_eq!(resolve(&layout, &both, Some("same-session"), None).unwrap().session_id, "same-session");
  }

  #[test]
  fn test_resolve_session_id_rejects_mismatches_without_env() {
    let layout = Layout::new();
    let cwd_dir = layout.session_dir("cwd-session");
    let error = resolve(&layout, &cwd_dir, Some("arg-session"), None).unwrap_err();
    assert_eq!(error.code, 2);
    let message = error.body["error"].as_str().unwrap();
    assert!(message.contains("mismatch"), "{message}");
    assert!(message.contains("arg-session") && message.contains("cwd-session"), "{message}");
    assert_eq!(
      message,
      "session id mismatch: --session=arg-session cwd=cwd-session; refusing to use an ambiguous session"
    );
  }

  #[test]
  fn a_cwd_outside_the_sessions_dir_supplies_nothing() {
    let layout = Layout::new();
    let error = resolve(&layout, &layout.outside(), None, None).unwrap_err();
    assert_eq!(error, CliExit::usage("--session required when not running from a CharlieBot session dir"));
  }

  #[test]
  fn test_resolve_session_id_env_outranks_cwd() {
    let layout = Layout::new();
    let outside = layout.outside();
    assert!(resolve(&layout, &outside, None, Some("env-session")).unwrap().note.is_none());
    let own = layout.session_dir("env-session");
    assert!(resolve(&layout, &own, None, Some("env-session")).unwrap().note.is_none());
    let other = layout.session_dir("other-session");
    let resolved = resolve(&layout, &other, None, Some("env-session")).unwrap();
    assert_eq!(resolved.session_id, "env-session");
    assert_eq!(
      resolved.note.as_deref(),
      Some("cwd is session dir of other-session; using CHARLIEBOT_SESSION_ID=env-session")
    );
    let with_arg = resolve(&layout, &other, Some("env-session"), Some("env-session")).unwrap();
    assert_eq!(with_arg.session_id, "env-session");
    assert_eq!(
      with_arg.note.as_deref(),
      Some("cwd is session dir of other-session; using CHARLIEBOT_SESSION_ID=env-session")
    );
  }

  #[test]
  fn the_env_variable_rejects_a_disagreeing_explicit_session() {
    let layout = Layout::new();
    let outside = layout.outside();
    let error = resolve(&layout, &outside, Some("arg-session"), Some("env-session")).unwrap_err();
    assert_eq!(error.code, 2);
    assert_eq!(
      error.body["error"].as_str().unwrap(),
      "session id mismatch: --session=arg-session CHARLIEBOT_SESSION_ID=env-session; refusing to use an ambiguous session"
    );
  }
}
