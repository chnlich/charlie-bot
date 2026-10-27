//! The `memory` CLI verbs (`src/features/memory/cli.py`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::pyfmt::str_repr;
use charliebot_core::{credentials, home, pystr, run_token};
use charliebot_store::run_identity;
use charliebot_store::run_records::RunRecord;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};
use unicode_width::UnicodeWidthChar;

const RUN_TOKEN_ENV: &str = "CHARLIEBOT_RUN_TOKEN";
const TEST_NOW_ENV: &str = "CHARLIEBOT_MEMORY_CLI_TEST_NOW";
const TOPICS_FILENAME: &str = "topics";
const ENTRIES_DIRNAME: &str = "entries";
const STAGING_DIRNAME: &str = "staging";
const BODY_LINE_COLUMN_LIMIT: usize = 120;

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
    return Err(CliExit::error("memory proposal is not ported to the Rust CLI yet"));
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
  let sessions_root = home_dir.join("sessions");
  let session_dir = sessions_root.join(&claims.session_id);
  let run_path = session_dir.join("data/runs").join(&claims.run_id).join("metadata.json");
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
  let events_path = session_dir.join("data/chat_events.jsonl");
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
  let text = match fs::read_to_string(path) {
    Ok(text) => text,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(CliExit::error(format!("read {}: {error}", path.display()))),
  };
  Ok(
    text
      .split('\n')
      .filter(|line| !line.is_empty())
      .filter_map(|line| serde_json::from_str::<Value>(line).ok())
      .filter(|event| event.get("type").and_then(Value::as_str) == Some("run_finished"))
      .collect(),
  )
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
    Some(path) => read_text(path)?,
    None => {
      let mut body = String::new();
      io::stdin().read_to_string(&mut body).map_err(|error| CliExit::error(error.to_string()))?;
      normalize_newlines(&body)
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
  let target = write_capture(&memory_dir, &home_dir.join("sessions"), &cwd, title, &body, cli_clock)?;
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
  let target = memory_dir.join(STAGING_DIRNAME).join(filename);
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
  let (violations, warnings) = lint_store(&root)?;
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

fn lint_store(root: &Path) -> Result<(Vec<String>, Vec<String>), CliExit> {
  let topics_path = root.join(TOPICS_FILENAME);
  let mut violations = Vec::new();
  let topics = if !topics_path.is_file() {
    violations.push(format!("{}: topics vocabulary file not found", topics_path.display()));
    BTreeMap::new()
  } else {
    match load_topics_for_lint(&topics_path)? {
      Ok(topics) => topics,
      Err(violation) => {
        violations.push(violation);
        BTreeMap::new()
      }
    }
  };
  let entry_files = entry_files(root)?;
  for path in &entry_files {
    match crate::memory::parse_entry(path) {
      Ok(entry) => violations.extend(validate_entry(&entry, &topics)),
      Err(error) if error.downcast_ref::<crate::memory::MemoryFormatError>().is_some() => {
        violations.push(error.to_string());
      }
      Err(error) => return Err(CliExit::error(error.to_string())),
    }
  }
  let warnings = width_warnings(root, &entry_files)?;
  let staging_dir = root.join(STAGING_DIRNAME);
  if staging_dir.is_dir() {
    for path in sorted_markdown_files(&staging_dir)? {
      let text = read_text(&path)?;
      let first_line = text.split('\n').next().unwrap_or_default();
      if text.is_empty() {
        violations.push(format!("{}: empty capture file", path.display()));
      } else if !first_line.starts_with("# ") {
        violations.push(format!("{}: line 1: capture must start with '# <title>'", path.display()));
      } else if pystr::strip(&first_line[2..]).is_empty() {
        violations.push(format!("{}: line 1: empty title after '# '", path.display()));
      }
    }
  }
  Ok((violations, warnings))
}

fn load_topics_for_lint(path: &Path) -> Result<Result<BTreeMap<String, bool>, String>, CliExit> {
  let text = read_text(path)?;
  let mut topics = BTreeMap::new();
  for (index, raw) in text.split('\n').enumerate() {
    if raw.is_empty() {
      continue;
    }
    let line_number = index + 1;
    let Some((name, resident)) = parse_topic_line(raw) else {
      return Ok(Err(format!("{}: line {line_number}: malformed topic line: {}", path.display(), str_repr(raw))));
    };
    if topics.contains_key(name) {
      return Ok(Err(format!("{}: line {line_number}: duplicate topic {}", path.display(), str_repr(name))));
    }
    topics.insert(name.to_string(), resident);
  }
  Ok(Ok(topics))
}

fn parse_topic_line(raw: &str) -> Option<(&str, bool)> {
  let (name, resident) = match raw.strip_suffix(" resident") {
    Some(name) => (name, true),
    None => (raw, false),
  };
  let mut chars = name.chars();
  chars.next().filter(|character| character.is_ascii_lowercase() || character.is_ascii_digit())?;
  chars
    .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
    .then_some((name, resident))
}

fn validate_entry(entry: &crate::memory::Entry, topics: &BTreeMap<String, bool>) -> Vec<String> {
  let topic_label = entry.topic.as_deref().unwrap_or("?");
  let violation = |message: String| format!("entries/{topic_label}/{}.md: {message}", entry.slug);
  let mut violations = Vec::new();
  if !is_slug(&entry.slug) {
    violations
      .push(violation(format!("filename slug {} does not match slug charset [A-Za-z0-9._-]", str_repr(&entry.slug))));
  }
  match entry.topic.as_deref() {
    None => violations.push(violation("missing required header field 'topic'".to_string())),
    Some(topic) if !is_topic_name(topic) => {
      violations.push(violation(format!("topic {} is not a valid topic name", str_repr(topic))));
    }
    Some(_) => {}
  }
  if let Some(topic) = entry.topic.as_deref().filter(|topic| !topics.contains_key(*topic)) {
    violations.push(violation(format!("topic {} not in topics vocabulary", str_repr(topic))));
  }
  let parent_name = entry.path.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy();
  if let Some(topic) = entry.topic.as_deref().filter(|topic| *topic != parent_name) {
    violations.push(violation(format!("directory name {} != topic {}", str_repr(&parent_name), str_repr(topic))));
  }
  if entry.scope.is_none() {
    violations.push(violation("missing required header field 'scope'".to_string()));
  }
  if entry.audience.is_none() {
    violations.push(violation("missing required header field 'audience'".to_string()));
  }
  if let Some(scope) = entry.scope.as_deref().filter(|scope| !["user", "host"].contains(scope)) {
    violations.push(violation(format!("scope {} not in {{user, host}}", str_repr(scope))));
  }
  for element in entry.audience.iter().flatten().filter(|element| !["master", "worker"].contains(&element.as_str())) {
    violations.push(violation(format!("audience element {} not in {{master, worker}}", str_repr(element))));
  }
  violations
}

fn entry_files(root: &Path) -> Result<Vec<PathBuf>, CliExit> {
  let entries_dir = root.join(ENTRIES_DIRNAME);
  if !entries_dir.is_dir() {
    return Ok(Vec::new());
  }
  let mut files = Vec::new();
  for topic_dir in sorted_children(&entries_dir)? {
    if topic_dir.is_dir() {
      files.extend(sorted_markdown_files(&topic_dir)?);
    }
  }
  Ok(files)
}

fn sorted_markdown_files(directory: &Path) -> Result<Vec<PathBuf>, CliExit> {
  Ok(
    sorted_children(directory)?
      .into_iter()
      .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().ends_with(".md")))
      .collect(),
  )
}

fn sorted_children(directory: &Path) -> Result<Vec<PathBuf>, CliExit> {
  let mut paths = fs::read_dir(directory)
    .map_err(|error| CliExit::error(format!("list {}: {error}", directory.display())))?
    .map(|entry| entry.map(|entry| entry.path()).map_err(|error| CliExit::error(error.to_string())))
    .collect::<Result<Vec<_>, _>>()?;
  paths.sort();
  Ok(paths)
}

fn width_warnings(root: &Path, files: &[PathBuf]) -> Result<Vec<String>, CliExit> {
  let mut warnings = Vec::new();
  for path in files {
    let text = read_text(path)?;
    let relative = path.strip_prefix(root).expect("entry files are under their store root").to_string_lossy();
    for line in entry_body(&text) {
      let width = display_width(line);
      if width > BODY_LINE_COLUMN_LIMIT {
        let preview: String = line.chars().take(60).collect();
        warnings.push(format!("{relative}: body line over {BODY_LINE_COLUMN_LIMIT} columns ({width}): {preview}"));
      }
    }
  }
  Ok(warnings)
}

fn entry_body(text: &str) -> Vec<&str> {
  let lines: Vec<&str> = text.split('\n').collect();
  if pystr::strip(lines[0]) != "---" {
    return lines.into_iter().filter(|line| !pystr::strip(line).is_empty()).collect();
  }
  for (index, line) in lines.iter().enumerate().skip(1) {
    if pystr::strip(line) == "---" {
      return lines[index + 1..].iter().copied().filter(|line| !pystr::strip(line).is_empty()).collect();
    }
  }
  Vec::new()
}

fn display_width(line: &str) -> usize {
  line.chars().map(|character| UnicodeWidthChar::width(character).unwrap_or(1).max(1)).sum()
}

fn is_slug(value: &str) -> bool {
  !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_topic_name(value: &str) -> bool {
  let mut chars = value.chars();
  chars.next().is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    && chars.all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
}

fn read_text(path: &Path) -> Result<String, CliExit> {
  let text = fs::read_to_string(path).map_err(|error| CliExit::error(format!("read {}: {error}", path.display())))?;
  Ok(normalize_newlines(&text))
}

fn normalize_newlines(text: &str) -> String {
  text.replace("\r\n", "\n").replace('\r', "\n")
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
      fs::write(memory_dir.join(TOPICS_FILENAME), "profile resident\n").unwrap();
      let body = "# Prefers Dark Mode\n\nThe user prefers dark themes across all UIs.\n";
      let target =
        write_capture(&memory_dir, &sessions_root, &cwd, "Prefers Dark Mode", body, || Ok(fixed_clock())).unwrap();
      let files = sorted_markdown_files(&memory_dir.join(STAGING_DIRNAME)).unwrap();
      assert_eq!(files.len(), 1);
      assert_eq!(target.file_name().unwrap(), "20261027T123456Z-12345678-prefers-dark-mode.md");
      assert_eq!(fs::read_to_string(&target).unwrap(), body);
      assert!(entry_files(&memory_dir).unwrap().is_empty());
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
      assert_eq!(fs::read_to_string(memory_dir.join(TOPICS_FILENAME)).unwrap(), crate::memory::DEFAULT_MEMORY_TOPICS);
      assert_eq!(fs::read_to_string(memory_dir.join(".gitignore")).unwrap(), crate::memory::DEFAULT_MEMORY_GITIGNORE);
      let files = sorted_markdown_files(&memory_dir.join(STAGING_DIRNAME)).unwrap();
      assert_eq!(files.as_slice(), std::slice::from_ref(&target));
      assert_eq!(fs::read_to_string(target).unwrap(), body);
    });
  }
}
