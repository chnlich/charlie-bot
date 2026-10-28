//! `home-helper scan`: the live runs and improve loops of a home, and each live run's re-attachment declaration in
//! the running and target builds. A run re-attaches after a restart only when its backend type's registration sets
//! `restart_reattach` in both builds.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::LazyLock;

use anyhow::{Context as _, Result};
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config;
use charliebot_core::pyfmt;
use charliebot_improve::command::{STATE_FILE, loops_dir_in};
use charliebot_store::layout;
use charliebot_store::run_records::RUN_METADATA_NAME;
use regex::Regex;
use serde_json::Value;

use crate::common::{exact, failed, python_repr, read_config, read_json};
use crate::process;

/// Each backend's registration has moved twice: the agents crate held it from 7b680fcbb until the crate grouping at
/// 03459e0ca, the runtime/agents crate from 03459e0ca until each backend's own crate at d1a07436f, and that crate from
/// d1a07436f on. A build's tree says which layout it has, so each sha is read in its own.
const BACKEND_LAYOUTS: [(&str, &str); 3] = [
  ("rust/crates/charliebot-agents/src/backends", "/mod.rs"),
  ("rust/crates/runtime/agents/src/backends", "/mod.rs"),
  ("rust/crates/backends", "/src/lib.rs"),
];

/// One registration in a backend module: its type name (a string or a const) and its trait block.
static REGISTRATION: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r#"(?s)type_name:\s*(?P<name>"[^"]+"|[A-Z_]+)\s*,\s*traits:\s*BackendTraits\s*\{(?P<body>.*?)\}"#)
    .expect("the registration pattern")
});

/// The `restart_reattach` value at the start of a registration's own line.
static REATTACH: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"(?m)^\s*restart_reattach:\s*(true|false)\s*,?\s*$").expect("the re-attach pattern"));

/// `scan HOME REPO RUNNING_IMPL TARGET_SHA`: one `live` line per live run, one `block` line per refusal.
pub fn scan(args: &[OsString]) -> Result<(), CliExit> {
  let [home, repo, running_impl, target_sha] = exact::<4>(args)?;
  let home = PathBuf::from(home);
  let repo = PathBuf::from(repo);
  let types = backend_types(&home)?;
  let boot = process::boot_time().map_err(failed)?;
  let source_sha = running_impl.strip_prefix("rust ").unwrap_or(&running_impl);
  let sessions = config::sessions_dir_in(&home);
  let mut registrations = Registrations::default();
  for session in sorted_directories(&sessions).map_err(failed)? {
    let runs = layout::runs_dir(&session);
    for run_dir in sorted_directories(&runs).map_err(failed)? {
      let metadata = run_dir.join(RUN_METADATA_NAME);
      if !metadata.is_file() {
        continue;
      }
      let run = read_json(&metadata).map_err(failed)?;
      if matches!(run.get("ended_at"), Some(value) if !value.is_null()) {
        continue;
      }
      let Some((pid, pid_start, backend)) = live_run(&run, boot).map_err(failed)? else {
        continue;
      };
      let id = text_field(&run, "id")?;
      let session_id = text_field(&run, "session_id")?;
      let display = pyfmt::python_str(&backend);
      println!("live\t{session_id}\t{id}\t{display}\t{pid}\t{pid_start}");
      let Some(backend_type) = backend.as_str().and_then(|backend| types.get(backend)) else {
        println!(
          "block\tbackend {} of run {id} (pid {pid}) is not in backends.options; restart re-attachment is unknown",
          python_repr(&backend)
        );
        continue;
      };
      for (label, sha) in [("running build", source_sha), ("target build", target_sha.as_str())] {
        let (enabled, detail) = registrations.lookup(&repo, sha, backend_type).map_err(failed)?;
        if enabled != Some(true) {
          let state = if enabled == Some(false) { "false" } else { "unknown" };
          println!(
            "block\t{display} run {id} (type {backend_type}, pid {pid}) cannot be re-attached after a restart: {label} \
             {sha} marks restart_reattach = {state} ({detail})"
          );
        }
      }
    }
  }
  // A loop's controller runs inside the server process; a restart never resumes it (rust/crates/runtime/tasks/src/recovery.rs).
  for session in sorted_directories(&sessions).map_err(failed)? {
    let Some(session_id) = session.file_name().and_then(|name| name.to_str()) else {
      continue;
    };
    for loop_dir in sorted_directories(&loops_dir_in(&home, session_id)).map_err(failed)? {
      let state_path = loop_dir.join(STATE_FILE);
      if !state_path.is_file() {
        continue;
      }
      let state = read_json(&state_path).map_err(failed)?;
      let running = state.get("status").and_then(Value::as_str) == Some("running");
      let Some(controller) = state.get("server_pid").and_then(Value::as_i64) else {
        continue;
      };
      if running && Path::new(&format!("/proc/{controller}")).exists() {
        println!(
          "block\timprove loop {} is running under pid {controller}",
          loop_dir.file_name().unwrap_or_default().to_string_lossy()
        );
      }
    }
  }
  Ok(())
}

/// The backend options' id-to-type map, which the scan uses to find a run's registered type.
fn backend_types(home: &Path) -> Result<HashMap<String, String>, CliExit> {
  let document = read_config(home).map_err(failed)?;
  let options = document
    .get("backends")
    .and_then(|backends| backends.get("options"))
    .and_then(Value::as_array)
    .ok_or_else(|| CliExit::error("config.yaml has no backends.options"))?;
  let mut types = HashMap::new();
  for option in options {
    let id = option.get("id").and_then(Value::as_str).ok_or_else(|| CliExit::error("a backend option has no id"))?;
    let backend_type =
      option.get("type").and_then(Value::as_str).ok_or_else(|| CliExit::error("a backend option has no type"))?;
    types.insert(id.to_string(), backend_type.to_string());
  }
  Ok(types)
}

/// A live run's recorded identity: `(pid, pid_start, backend)`. A run whose process has ended or started before this
/// boot is not live.
fn live_run(run: &Value, boot: i64) -> Result<Option<(u32, String, Value)>> {
  let (Some(pid), Some(pid_start), Some(started_at)) = (
    run.get("pid").and_then(Value::as_u64),
    run.get("pid_start").and_then(Value::as_str),
    run.get("started_at").and_then(Value::as_str),
  ) else {
    return Ok(None);
  };
  let Ok(pid) = u32::try_from(pid) else {
    return Ok(None);
  };
  if !process::instance(pid, pid_start) || !process::started_after_boot(started_at, boot)? {
    return Ok(None);
  }
  Ok(Some((pid, pid_start.to_string(), run.get("backend").cloned().unwrap_or(Value::Null))))
}

/// One run record's required text field.
fn text_field<'a>(run: &'a Value, name: &str) -> Result<&'a str, CliExit> {
  run.get(name).and_then(Value::as_str).ok_or_else(|| CliExit::error(format!("a run record has no {name}")))
}

/// The directories under `path` in file-name order; a missing directory holds none.
fn sorted_directories(path: &Path) -> std::io::Result<Vec<PathBuf>> {
  let mut directories = Vec::new();
  let entries = match fs::read_dir(path) {
    Ok(entries) => entries,
    Err(error) if error.kind() == ErrorKind::NotFound => return Ok(directories),
    Err(error) => return Err(error),
  };
  for entry in entries {
    let entry = entry?;
    if entry.file_type()?.is_dir() {
      directories.push(entry.path());
    }
  }
  directories.sort();
  Ok(directories)
}

/// One git command in the repository; a spawn failure is an error, a command's own failure is the caller's.
fn git(repo: &Path, args: &[&str]) -> Result<Output> {
  Command::new("git").arg("-C").arg(repo).args(args).output().with_context(|| format!("run git {args:?}"))
}

/// The registration files of one build in the layout its tree has, and the error a failed tree read gave.
fn registration_files(repo: &Path, sha: &str) -> Result<(Vec<String>, Option<String>)> {
  for (prefix, suffix) in BACKEND_LAYOUTS {
    let listed = git(repo, &["ls-tree", "-r", "--name-only", sha, prefix])?;
    if !listed.status.success() {
      let stderr = String::from_utf8_lossy(&listed.stderr);
      return Ok((Vec::new(), Some(format!("cannot inspect build {sha}: {}", stderr.trim()))));
    }
    let paths: Vec<String> = String::from_utf8_lossy(&listed.stdout)
      .lines()
      .filter(|path| path.ends_with(suffix))
      .map(str::to_string)
      .collect();
    if !paths.is_empty() {
      return Ok((paths, None));
    }
  }
  Ok((Vec::new(), None))
}

/// One build's re-attachment declaration for a backend type: the value when the build declares it, and the reason when
/// it does not.
#[derive(Default)]
struct Registrations {
  cache: HashMap<(String, String), (Option<bool>, String)>,
}

impl Registrations {
  /// The declaration of `backend_type` in build `sha`, read from that build's tree once per build and type.
  fn lookup(&mut self, repo: &Path, sha: &str, backend_type: &str) -> Result<(Option<bool>, String)> {
    let key = (sha.to_string(), backend_type.to_string());
    if let Some(hit) = self.cache.get(&key) {
      return Ok(hit.clone());
    }
    let result = read_registration(repo, sha, backend_type)?;
    self.cache.insert(key, result.clone());
    Ok(result)
  }
}

/// Read one build's registration without checking out or changing its source tree.
fn read_registration(repo: &Path, sha: &str, backend_type: &str) -> Result<(Option<bool>, String)> {
  let (paths, error) = registration_files(repo, sha)?;
  if let Some(error) = error {
    return Ok((None, error));
  }
  for path in paths {
    let shown = git(repo, &["show", &format!("{sha}:{path}")])?;
    if !shown.status.success() {
      let stderr = String::from_utf8_lossy(&shown.stderr);
      return Ok((None, format!("cannot read {path} from build {sha}: {}", stderr.trim())));
    }
    let source = String::from_utf8_lossy(&shown.stdout);
    for registration in REGISTRATION.captures_iter(&source) {
      let name = &registration["name"];
      let Some(declared) = declared_name(&source, name) else {
        continue;
      };
      if declared != backend_type {
        continue;
      }
      return Ok(match REATTACH.captures(&registration["body"]) {
        Some(enabled) => (Some(&enabled[1] == "true"), path),
        None => (None, format!("{path} in build {sha} has no restart_reattach value")),
      });
    }
  }
  Ok((None, format!("build {sha} has no registration for backend type {backend_type}")))
}

/// The type name a registration spells: a quoted string as written, or the value of the const it names.
fn declared_name(source: &str, name: &str) -> Option<String> {
  if let Some(quoted) = name.strip_prefix('"') {
    return Some(quoted.strip_suffix('"')?.to_string());
  }
  let constant = Regex::new(&format!(r#"const\s+{}\s*:\s*&str\s*=\s*"([^"]+)""#, regex::escape(name))).ok()?;
  Some(constant.captures(source)?.get(1)?.as_str().to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_quoted_and_a_const_type_name_are_read() {
    let source = "pub const TYPE_NAME: &str = \"codex\";\nregister(TypeRegistration { type_name: TYPE_NAME, \
                  traits: BackendTraits { restart_reattach: true, }, });";
    assert_eq!(declared_name(source, "TYPE_NAME").as_deref(), Some("codex"));
    assert_eq!(declared_name(source, "\"opencode\"").as_deref(), Some("opencode"));
  }

  #[test]
  fn a_registration_carries_its_type_and_re_attach_value() {
    let source = "type_name: \"antigravity\",\n  traits: BackendTraits {\n    restart_reattach: false,\n  },";
    let registration = REGISTRATION.captures(source).expect("the registration");
    assert_eq!(&registration["name"], "\"antigravity\"");
    assert_eq!(&REATTACH.captures(&registration["body"]).expect("the value")[1], "false");
  }
}
