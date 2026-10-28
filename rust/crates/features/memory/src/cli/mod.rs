//! The `memory` CLI verbs (`src/features/memory/cli.py`).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_core::run_token::RUN_TOKEN_ENV;
use charliebot_core::{config, credentials, event_types, home, pystr, run_token};
use charliebot_store::chat_events::{self, LineRule};
use charliebot_store::run_records::RunRecord;
use charliebot_store::{layout, run_identity};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

mod digest;
mod proposal;
mod proposal_check;
mod store;

const TEST_NOW_ENV: &str = "CHARLIEBOT_MEMORY_CLI_TEST_NOW";
#[derive(Parser)]
#[command(name = "memory", about = "Labeled-entry memory store: query, add captures, and lint")]
struct MemoryCli {
  #[command(subcommand)]
  command: MemoryCommand,
}

#[derive(Subcommand)]
enum MemoryCommand {
  /// Print matched entries' full text or index lines.
  Query(QueryArgs),
  /// Stage a free-form capture.
  Add(AddArgs),
  /// Validate the store.
  Lint(LintArgs),
  /// Print the cross-session user-message digest for the memory curator.
  UserDigest,
}

#[derive(Args)]
struct QueryArgs {
  /// Topic to match (repeatable); must exist in the vocabulary.
  #[arg(long, required = true, action = clap::ArgAction::Append)]
  topic: Vec<String>,
  /// Only entries whose audience contains this value.
  #[arg(long, value_parser = ["master", "worker"])]
  audience: Option<String>,
  /// Print index lines only.
  #[arg(long)]
  index: bool,
  /// Only entries in resident topics.
  #[arg(long)]
  resident: bool,
  /// Store root to read.
  #[arg(long)]
  dir: Option<PathBuf>,
}

#[derive(Args)]
struct AddArgs {
  /// Read the capture body from a file; stdin is used by default.
  #[arg(long)]
  file: Option<PathBuf>,
}

#[derive(Args)]
struct LintArgs {
  /// Store root to validate.
  #[arg(long)]
  dir: Option<PathBuf>,
}

/// Parse the memory arguments and run one local store verb.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  if args.first().and_then(|arg| arg.to_str()) == Some("proposal") {
    return proposal::main(&args[1..]);
  }
  let command = with_infer_long_args(MemoryCli::command());
  let argv = std::iter::once(OsString::from("charliebot memory")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = MemoryCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    MemoryCommand::Query(args) => query(&args),
    MemoryCommand::Add(args) => add(&args),
    MemoryCommand::Lint(args) => lint(&args),
    MemoryCommand::UserDigest => digest::run(),
  }
}

fn query(args: &QueryArgs) -> Result<(), CliExit> {
  let token = std::env::var(RUN_TOKEN_ENV).unwrap_or_default();
  let fixed_audience = if token.trim().is_empty() {
    None
  } else {
    let home_dir = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
    Some(run_scoped_audience(&home_dir, token.trim())?)
  };
  if let Some(fixed) = fixed_audience.as_deref()
    && args.audience.as_deref().is_some_and(|audience| audience != fixed)
  {
    python_error(&format!(
      "error: --audience {} contradicts the run credential's fixed audience '{fixed}'; a run token cannot broaden its own query",
      args.audience.as_deref().expect("the audience mismatch has a requested audience")
    ));
  }
  let audience = fixed_audience.as_deref().or(args.audience.as_deref());
  let memory_dir = match args.dir.as_deref().filter(|path| !path.as_os_str().is_empty()) {
    Some(path) => PathBuf::from(charliebot_core::paths::expanduser(&path.to_string_lossy())),
    None => home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?.join("memory"),
  };
  let store = crate::memory::load_store(&memory_dir).map_err(|error| CliExit::error(error.to_string()))?;
  let unknown: Vec<&str> =
    args.topic.iter().map(String::as_str).filter(|topic| !store.topics.contains_key(*topic)).collect();
  if !unknown.is_empty() {
    for value in unknown {
      let topic_hint = value.split_once('/').map(|(topic, _)| topic).filter(|topic| store.topics.contains_key(*topic));
      match topic_hint {
        Some(topic) => {
          python_error_line(&format!("error: unknown topic: {value} (index lines are topic/slug; try --topic {topic})"))
        }
        None => python_error_line(&format!("error: unknown topic: {value}")),
      }
    }
    process::exit(1);
  }
  let wanted: BTreeSet<&str> = args.topic.iter().map(String::as_str).collect();
  let residents = crate::memory::resident_topic_names(&store);
  let mut matched: Vec<_> = store
    .entries
    .iter()
    .filter(|entry| entry.topic.as_deref().is_some_and(|topic| wanted.contains(topic)))
    .filter(|entry| audience.is_none_or(|value| crate::memory::audience_allows(entry, value)))
    .filter(|entry| !args.resident || entry.topic.as_deref().is_some_and(|topic| residents.contains(topic)))
    .collect();
  matched.sort_by(|left, right| crate::memory::entry_order_key(left).cmp(&crate::memory::entry_order_key(right)));
  if args.index {
    for entry in matched {
      println!("{} · {}", entry.id(), entry.title);
    }
  } else {
    for entry in matched {
      println!("{}", crate::memory::full_text(entry));
    }
  }
  Ok(())
}

fn run_scoped_audience(home_dir: &Path, token: &str) -> Result<String, CliExit> {
  let credentials = credentials::load_credentials(home_dir).map_err(|error| CliExit::error(error.to_string()))?;
  let key = credentials.access_key().unwrap_or_default();
  let claims = match run_token::verify_run_token(token, &key) {
    Ok(claims) => claims,
    Err(error) => python_error(&format!("error: invalid run token: {error}")),
  };
  let sessions_root = config::sessions_dir_in(home_dir);
  let session_dir = sessions_root.join(&claims.session_id);
  let run_path = layout::runs_dir(&session_dir).join(&claims.run_id).join(METADATA_FILE_NAME);
  let identity = run_identity::read_run_identity_sync(&run_path).map_err(|error| CliExit::error(error.to_string()))?;
  let Some(identity) = identity else {
    python_error(&format!("error: {}", run_identity::RUN_IDENTITY_UNKNOWN_DETAIL));
  };
  let run: RunRecord = serde_json::from_value(json!({
    "id": identity.id,
    "session_id": claims.session_id,
    "pid": identity.pid,
    "pid_start": identity.pid_start,
  }))
  .map_err(|error| CliExit::error(error.to_string()))?;
  let events_path = layout::chat_events_path(&session_dir);
  let events = read_run_events(&events_path)?;
  if let Some(refusal) = run_identity::run_identity_refusal(Some(&run), &events) {
    python_error(&format!("error: {refusal}"));
  }
  let metadata = load_json_metadata(&sessions_root.join(&claims.session_id).join(run_identity::SESSION_METADATA_NAME))?;
  let profile = metadata.as_ref().and_then(|value| value.get("profile")).and_then(Value::as_str);
  match profile {
    Some("manager") => Ok("master".to_string()),
    Some("worker") => Ok("worker".to_string()),
    _ => python_error(&format!(
      "error: run token references session {}, which is not a task-tree node in this instance",
      claims.session_id
    )),
  }
}

fn read_run_events(path: &Path) -> Result<Vec<Value>, CliExit> {
  let lines = match chat_events::read_lines(path, LineRule::Lines) {
    Ok(lines) => lines,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(CliExit::error(format!("read {}: {error}", path.display()))),
  };
  let mut run_finished = Vec::new();
  for line in lines {
    let line = line.map_err(|error| CliExit::error(format!("read {}: {error}", path.display())))?;
    // A line that is not JSON, and a JSON value that is not an object, names no event.
    let Ok(event) = line.decode() else { continue };
    if event.get("type").and_then(Value::as_str) == Some(event_types::RUN_FINISHED) {
      run_finished.push(event);
    }
  }
  Ok(run_finished)
}

fn load_json_metadata(path: &Path) -> Result<Option<Value>, CliExit> {
  if !path.exists() {
    return Ok(None);
  }
  let text = fs::read_to_string(path).map_err(|error| CliExit::error(format!("read {}: {error}", path.display())))?;
  Ok(serde_json::from_str(&text).ok())
}

fn add(args: &AddArgs) -> Result<(), CliExit> {
  let body = match &args.file {
    Some(path) => store::read_text(path)?,
    None => {
      let mut body = String::new();
      io::stdin().read_to_string(&mut body).map_err(|error| CliExit::error(error.to_string()))?;
      store::normalize_newlines(&body)
    }
  };
  let first_line = body.split('\n').next().unwrap_or_default();
  if !first_line.starts_with("# ") {
    python_error("error: body must start with '# <title>'");
  }
  let title = pystr::strip(&first_line[2..]);
  if title.is_empty() {
    python_error("error: empty title after '# '");
  }
  let home_dir = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let cwd = std::env::current_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let memory_dir = home_dir.join("memory");
  let target = write_capture(&memory_dir, &config::sessions_dir_in(&home_dir), &cwd, title, &body, cli_clock)?;
  println!("{}", target.display());
  Ok(())
}

fn write_capture(
  memory_dir: &Path,
  sessions_root: &Path,
  cwd: &Path,
  title: &str,
  body: &str,
  clock: impl FnOnce() -> Result<OffsetDateTime, CliExit>,
) -> Result<PathBuf, CliExit> {
  crate::memory::ensure_store(memory_dir).map_err(|error| CliExit::error(error.to_string()))?;
  let timestamp = clock()?
    .to_offset(UtcOffset::UTC)
    .format(format_description!("[year][month][day]T[hour][minute][second]Z"))
    .map_err(|error| CliExit::error(error.to_string()))?;
  let session_slug = session_slug8(sessions_root, cwd);
  let slug = slugify(title);
  let slug = if slug.is_empty() { "capture" } else { &slug };
  let filename = format!("{timestamp}-{session_slug}-{slug}.md");
  let target = memory_dir.join(store::STAGING_DIRNAME).join(filename);
  fs::write(&target, body).map_err(|error| CliExit::error(format!("write {}: {error}", target.display())))?;
  Ok(target)
}

fn cli_clock() -> Result<OffsetDateTime, CliExit> {
  match std::env::var(TEST_NOW_ENV) {
    Ok(value) => {
      OffsetDateTime::parse(&value, &Rfc3339).map_err(|error| CliExit::error(format!("{TEST_NOW_ENV}: {error}")))
    }
    Err(std::env::VarError::NotPresent) => Ok(OffsetDateTime::now_utc()),
    Err(error) => Err(CliExit::error(format!("{TEST_NOW_ENV}: {error}"))),
  }
}

fn session_slug8(sessions_root: &Path, cwd: &Path) -> String {
  // Python compares against `sessions_dir.resolve()`: a symlinked CHARLIEBOT_HOME must not hide
  // the session directory, since the cwd a process reads is already symlink-free.
  let sessions_root = &fs::canonicalize(sessions_root).unwrap_or_else(|_| sessions_root.to_path_buf());
  if cwd.parent() == Some(sessions_root) {
    cwd.file_name().map(|name| name.to_string_lossy().chars().take(8).collect()).unwrap_or_else(|| "nosess".to_string())
  } else {
    "nosess".to_string()
  }
}

fn slugify(title: &str) -> String {
  let mut slug = String::new();
  let mut previous_dash = false;
  for character in title.to_lowercase().chars() {
    if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
      if character == '-' && previous_dash {
        continue;
      }
      slug.push(character);
      previous_dash = character == '-';
    } else if !previous_dash {
      slug.push('-');
      previous_dash = true;
    }
  }
  slug.trim_matches('-').to_string()
}

fn lint(args: &LintArgs) -> Result<(), CliExit> {
  let root = match args.dir.as_deref().filter(|path| !path.as_os_str().is_empty()) {
    Some(path) => PathBuf::from(charliebot_core::paths::expanduser(&path.to_string_lossy())),
    None => home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?.join("memory"),
  };
  let (violations, warnings) = store::lint_store(&root)?;
  for warning in warnings {
    eprintln!("warning: {warning}");
  }
  if violations.is_empty() {
    println!("clean");
  } else {
    for violation in violations {
      println!("{violation}");
    }
    process::exit(1);
  }
  Ok(())
}

fn python_error(message: &str) -> ! {
  python_error_line(message);
  process::exit(1)
}

fn python_error_line(message: &str) {
  eprintln!("{message}");
}

#[cfg(test)]
mod tests {
  use std::sync::mpsc;
  use std::time::Duration;

  use super::*;

  fn run_with_timeout(test: impl FnOnce() + Send + 'static) {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
      test();
      let _ = sender.send(());
    });
    receiver.recv_timeout(Duration::from_secs(2)).expect("the test finishes within two seconds");
  }

  fn fixed_clock() -> OffsetDateTime {
    OffsetDateTime::parse("2026-10-27T12:34:56Z", &Rfc3339).expect("the test timestamp parses")
  }

  #[test]
  fn test_session_slug8_resolves_a_symlinked_sessions_root() {
    run_with_timeout(|| {
      let temp = tempfile::tempdir().unwrap();
      let real = temp.path().join("real-home").join("sessions").join("12345678-session-id");
      fs::create_dir_all(&real).unwrap();
      let linked = temp.path().join("linked-home");
      std::os::unix::fs::symlink(temp.path().join("real-home"), &linked).unwrap();
      assert_eq!(session_slug8(&linked.join("sessions"), &real), "12345678");
    });
  }

  #[test]
  fn test_cli_add_creates_one_staging_file() {
    run_with_timeout(|| {
      let temp = tempfile::tempdir().unwrap();
      let home_dir = temp.path().join("home");
      let memory_dir = home_dir.join("memory");
      let sessions_root = home_dir.join("sessions");
      let cwd = sessions_root.join("12345678-session-id");
      fs::create_dir_all(&cwd).unwrap();
      fs::create_dir_all(&memory_dir).unwrap();
      fs::write(memory_dir.join(store::TOPICS_FILENAME), "profile resident\n").unwrap();
      let body = "# Prefers Dark Mode\n\nThe user prefers dark themes across all UIs.\n";
      let target =
        write_capture(&memory_dir, &sessions_root, &cwd, "Prefers Dark Mode", body, || Ok(fixed_clock())).unwrap();
      let files = store::sorted_markdown_files(&memory_dir.join(store::STAGING_DIRNAME)).unwrap();
      assert_eq!(files.len(), 1);
      assert_eq!(target.file_name().unwrap(), "20261027T123456Z-12345678-prefers-dark-mode.md");
      assert_eq!(fs::read_to_string(&target).unwrap(), body);
      assert!(store::entry_files(&memory_dir).unwrap().is_empty());
    });
  }

  #[test]
  fn test_cli_add_on_a_fresh_home_creates_the_scaffold_and_the_capture() {
    run_with_timeout(|| {
      let temp = tempfile::tempdir().unwrap();
      let home_dir = temp.path().join("home");
      fs::create_dir_all(&home_dir).unwrap();
      let memory_dir = home_dir.join("memory");
      let sessions_root = home_dir.join("sessions");
      let cwd = temp.path().to_path_buf();
      let body = "# Fresh Capture\n\nA fact to record.\n";
      let target =
        write_capture(&memory_dir, &sessions_root, &cwd, "Fresh Capture", body, || Ok(fixed_clock())).unwrap();
      for name in [".git", "topics", ".gitignore", "entries", "staging"] {
        assert!(memory_dir.join(name).exists(), "{name}");
      }
      assert_eq!(
        fs::read_to_string(memory_dir.join(store::TOPICS_FILENAME)).unwrap(),
        crate::memory::DEFAULT_MEMORY_TOPICS
      );
      assert_eq!(fs::read_to_string(memory_dir.join(".gitignore")).unwrap(), crate::memory::DEFAULT_MEMORY_GITIGNORE);
      let files = store::sorted_markdown_files(&memory_dir.join(store::STAGING_DIRNAME)).unwrap();
      assert_eq!(files.as_slice(), std::slice::from_ref(&target));
      assert_eq!(fs::read_to_string(target).unwrap(), body);
    });
  }
}
