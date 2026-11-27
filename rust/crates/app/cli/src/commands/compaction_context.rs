//! The Claude SessionStart compaction hook.
//!
//! `compaction-context --session SID --slot N` re-attaches what a session carried before Claude
//! compacted. A manager session (no `--run`) gets the skills it read: the command scans
//! `sessions/<sid>/data/chat_events.jsonl` for Skill, Read and Bash tool calls that name a skill
//! directory, reads each skill's current `SKILL.md`, and splits the re-attached text into at most six
//! 9,000-character parts at line ends. The first part opens with the session memo sentence, so the new
//! context names the memo even when the session read no skill. A skill too large to fit is listed in
//! the first part instead of being partially attached; a skill whose file no longer resolves is
//! reported on stderr as unresolved.
//!
//! A worker run (`--run RID`) gets its task spec first: the first part opens with the task header, then
//! the text of `sessions/<sid>/data/runs/<rid>/task_spec.md`, then the skills as above. A run that
//! pinned no task spec gets skills only. A task spec that does not fit in the six parts is cut at a
//! character, and the last part then carries one line with the restored count, the total count and the
//! file's full path.
//!
//! A failure (unreadable log, invalid event JSON, unreadable skill or task spec) prints the error line
//! and exits 1; a slot past the last part prints only the stderr line.
//!
//! `compaction-context --memo-only` prints the memo sentence alone. The Codex backend installs it
//! as the SessionStart compact hook of a manager turn, which re-attaches no skills.
//!
//! The repository's `skills/` directory comes from `CHARLIEBOT_REPO_ROOT`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use charliebot_claude_code::claude_launch;
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::json_out;
use charliebot_core::config;
use charliebot_core::event_types::{ASSISTANT, READ_TOOL, SKILL_TOOL, TOOL_INPUT_FILE_PATH, TOOL_USE};
use charliebot_core::home;
use charliebot_core::json_loads;
use charliebot_core::paths;
use charliebot_core::regex_classes::compile_regex;
use charliebot_core::repo_root;
use charliebot_core::session_memo::SESSION_MEMO_NOTE;
use charliebot_core::str_methods::universal_newlines;
use charliebot_core::text_repr::{is_truthy, str_repr};
use charliebot_store::chat_events::{self, LineRule, LogReadError};
use charliebot_store::layout;
use charliebot_store::run_records::RUN_TASK_SPEC_NAME;
use clap::{CommandFactory, FromArgMatches, Parser};
use fancy_regex::Regex;
use serde_json::{Value, json};

/// One hook part holds at most this many characters.
const MAX_PART_CHARS: usize = 9000;

/// The split yields at most one part per slot the SessionStart hook installs.
const MAX_PARTS: usize = claude_launch::COMPACTION_CONTEXT_SLOTS;

/// The line that opens a worker run's restored task spec.
const TASK_SPEC_HEADER: &str = "This run's task as delegated, restored after compaction:";

#[derive(Parser)]
#[command(name = "compaction-context", about = "Restore skills read by a Claude manager session after compaction")]
struct CompactionContextCli {
  /// CharlieBot session id; the skill mode requires it
  #[arg(long, required_unless_present = "memo_only")]
  session: Option<String>,
  /// Part number, from 1 to the installed slot count; the skill mode requires it
  #[arg(long, required_unless_present = "memo_only", value_parser = clap::value_parser!(i64).range(1..=MAX_PARTS as i64))]
  slot: Option<i64>,
  /// Print the session memo sentence alone, with no re-attached skill text
  #[arg(long)]
  memo_only: bool,
  /// Worker run whose pinned task spec is restored ahead of its skills; a manager's hook names no run
  #[arg(long)]
  run: Option<String>,
}

/// One skill read the log records: the skill name, the concrete `.../skills/<name>/SKILL.md` path
/// the Read or Bash event named (`None` for a Skill tool call), the absolute `cd` target of the
/// Bash command, and the read's instant and line order.
struct SkillRead {
  name: String,
  named_path: Option<String>,
  command_cwd: Option<PathBuf>,
  /// Microseconds since the epoch of the aware timestamp; aware datetimes order by instant.
  read_at: i64,
  order: usize,
}

/// One skill's current file and text, in the order the hook re-attaches them.
#[derive(Clone)]
struct SkillText {
  name: String,
  path: PathBuf,
  text: String,
}

/// Parse the arguments and run one slot. Parser errors print and exit on their own: help exits 0,
/// a usage error exits 2.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(CompactionContextCli::command());
  let argv = std::iter::once(OsString::from("charliebot compaction-context")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = CompactionContextCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  if cli.memo_only {
    print!("{}", json_out::render(&hook_json(SESSION_MEMO_NOTE), None));
    return Ok(());
  }
  let session = cli.session.as_deref().expect("the skill mode requires a session id");
  let slot = cli.slot.expect("the skill mode requires a slot");
  if let Err(error) = run(session, slot, cli.run.as_deref()) {
    eprintln!("session={session} slot={slot} characters=0 skills=- error={error}");
    std::process::exit(1);
  }
  Ok(())
}

fn run(session: &str, slot: i64, run_id: Option<&str>) -> Result<(), String> {
  let home = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let repo = repo_root::RepoRoot::new(std::env::var_os(repo_root::REPO_ROOT_ENV))
    .map_err(|error| error.to_string())?
    .path()
    .to_path_buf();
  let session_dir = config::sessions_dir_in(&home).join(session);
  let events_path = layout::chat_events_path(&session_dir);
  let restore = match run_id {
    Some(run_id) => Restore::Task(read_task_spec(&session_dir, run_id)?),
    None => Restore::Memo,
  };
  let (skills, unresolved) = load_skill_texts(&events_path, &home, &repo)?;
  let (parts, included, omitted) = context_parts(&restore, &skills)?;
  let part = if slot <= parts.len() as i64 { parts[(slot - 1) as usize].clone() } else { String::new() };
  if !part.is_empty() {
    print!("{}", json_out::render(&hook_json(&part), None));
  }
  let names = |skills: &[SkillText]| {
    if skills.is_empty() {
      "-".to_string()
    } else {
      skills.iter().map(|skill| skill.name.clone()).collect::<Vec<_>>().join(",")
    }
  };
  eprintln!(
    "session={session} slot={slot} characters={} skills={} omitted={} unresolved={}",
    part.chars().count(),
    names(&included),
    names(&omitted),
    if unresolved.is_empty() { "-".to_string() } else { unresolved.join(",") },
  );
  Ok(())
}

fn hook_json(part: &str) -> Value {
  json!({
    "hookSpecificOutput": {
      "hookEventName": claude_launch::SESSION_START_HOOK_EVENT,
      "additionalContext": part
    }
  })
}

// ---------------------------------------------------------------------------
// The chat-events log
// ---------------------------------------------------------------------------

fn latest_skill_reads(events_path: &Path) -> Result<Vec<SkillRead>, String> {
  // Each physical line keeps its terminator (`LineRule::Terminated`), so a JSON error position
  // counts the same bytes the log line holds.
  // A name's first position is kept and a later read overwrites its value. Reads from one event
  // share a key, and the stable sort keeps their order, so a `Vec` in first-seen order gives the
  // same answer; a `HashMap` would reorder them from run to run.
  let mut latest: Vec<SkillRead> = Vec::new();
  let lines =
    chat_events::read_lines(events_path, LineRule::Terminated).map_err(|error| log_read_error(events_path, error))?;
  for line in lines {
    let line = line.map_err(|error| log_read_error(events_path, error))?;
    let event = line.decode().map_err(|error| json_error(&line.text, &error))?;
    let Some(message) = assistant_message(&event) else { continue };
    let Some(content) = message.get("content").and_then(Value::as_array) else { continue };
    for block in content {
      let Some(block) = block.as_object() else { continue };
      if block.get("type").and_then(Value::as_str) != Some(TOOL_USE) {
        continue;
      }
      for (name, named_path, command_cwd) in reads_from_tool(block) {
        let read = SkillRead { read_at: timestamp(&event)?, order: line.number, name, named_path, command_cwd };
        match latest.iter_mut().find(|previous| previous.name == read.name) {
          Some(previous) => {
            if (read.read_at, read.order) > (previous.read_at, previous.order) {
              *previous = read;
            }
          }
          None => latest.push(read),
        }
      }
    }
  }
  latest.sort_by_key(|read| std::cmp::Reverse((read.read_at, read.order)));
  Ok(latest)
}

/// The event's `message` when the event is an assistant message with a mapping payload.
fn assistant_message(event: &Value) -> Option<&Value> {
  if event.get("type").and_then(Value::as_str) != Some(ASSISTANT) {
    return None;
  }
  let message = event.get("message")?;
  message.as_object()?;
  Some(message)
}

fn timestamp(event: &Value) -> Result<i64, String> {
  match event.get("timestamp") {
    Some(Value::String(raw)) => parse_iso_moment(raw),
    _ => Err("skill tool call has no timestamp".to_string()),
  }
}

/// The reads one tool_use block records: `(name, named path, command cwd)`.
fn reads_from_tool(block: &serde_json::Map<String, Value>) -> Vec<(String, Option<String>, Option<PathBuf>)> {
  let Some(tool_input) = block.get("input").and_then(Value::as_object) else { return Vec::new() };
  match block.get("name").and_then(Value::as_str) {
    Some(SKILL_TOOL) => {
      let skill = or_value(tool_input.get("skill"), tool_input.get("skill_name"));
      match skill {
        Some(Value::String(skill_name)) if !skill_name.is_empty() => vec![(skill_name.clone(), None, None)],
        _ => Vec::new(),
      }
    }
    Some(READ_TOOL) => {
      let Some(Value::String(file_path)) = tool_input.get(TOOL_INPUT_FILE_PATH) else { return Vec::new() };
      if posix_name(file_path) != "SKILL.md" {
        return Vec::new();
      }
      skill_locations(file_path)
        .into_iter()
        .map(|(name, directory)| (name, Some(named_path(&directory)), None))
        .collect()
    }
    Some("Bash") => {
      let Some(Value::String(command)) = tool_input.get("command") else { return Vec::new() };
      if !command.contains("SKILL.md") {
        return Vec::new();
      }
      let cwd = command_cwd(command);
      skill_locations(command)
        .into_iter()
        .map(|(name, directory)| (name, Some(named_path(&directory)), cwd.clone()))
        .collect()
    }
    _ => Vec::new(),
  }
}

/// `f"{directory}/SKILL.md"` — the concrete skill file the Read or Bash event named.
fn named_path(directory: &str) -> String {
  format!("{directory}/SKILL.md")
}

/// The truthy `left` value, otherwise `right`, over two optional JSON values.
fn or_value<'a>(left: Option<&'a Value>, right: Option<&'a Value>) -> Option<&'a Value> {
  match left {
    Some(value) if is_truthy(value) => Some(value),
    _ => right,
  }
}

/// The final path component, with `\` read as `/`.
fn posix_name(text: &str) -> String {
  let replaced = text.replace('\\', "/");
  replaced.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

fn skill_location_regex() -> &'static Regex {
  static REGEX: OnceLock<Regex> = OnceLock::new();
  REGEX.get_or_init(|| {
    Regex::new(concat!(
      r#"(?<![A-Za-z0-9_-])(?P<directory>[^ \t\n'"`;&|()]*?skills[/\\]"#,
      r#"(?P<name>[^/\\ \t\n'"`;&|()]+))(?=$|[/\\ \t\n'"`;&|(),])"#,
    ))
    .expect("the ported _SKILL_LOCATION expression is valid")
  })
}

fn skill_name_regex() -> &'static Regex {
  static REGEX: OnceLock<Regex> = OnceLock::new();
  REGEX.get_or_init(|| Regex::new("^[A-Za-z0-9][A-Za-z0-9._-]*$").expect("the ported _SKILL_NAME expression is valid"))
}

/// The `(.../skills/<name>)` locations one text names, latest-scan order: the directory group
/// carries the name, so it is the concrete skill directory the event wrote.
fn skill_locations(value: &str) -> Vec<(String, String)> {
  let mut locations = Vec::new();
  let captures = skill_location_regex().captures_iter(value).filter_map(|found| found.ok());
  for found in captures {
    let name = found.name("name").map(|found| found.as_str()).unwrap_or_default();
    if !skill_name_regex().is_match(name).unwrap_or(false) {
      continue;
    }
    locations.push((
      name.to_string(),
      found.name("directory").map(|found| found.as_str()).unwrap_or_default().replace('\\', "/"),
    ));
  }
  locations
}

fn command_cwd_regex() -> &'static Regex {
  static REGEX: OnceLock<Regex> = OnceLock::new();
  REGEX.get_or_init(|| compile_regex(r#"^\s*cd\s+('[^']*'|"[^"]*"|[^;&|\s]+)(?:\s*(?:&&|;|$))"#))
}

/// The absolute `cd` target of a Bash command, `None` when the command does not open with one.
fn command_cwd(command: &str) -> Option<PathBuf> {
  let captures = command_cwd_regex().captures(command).ok().flatten()?;
  let raw_path = captures.name("path")?.as_str().trim_matches(|character| character == '\'' || character == '"');
  let expanded = paths::expanduser(&expandvars(raw_path));
  if Path::new(&expanded).is_absolute() { Some(PathBuf::from(expanded)) } else { None }
}

/// `$NAME` and `${NAME}` become the environment value, and an unset or malformed reference stays
/// as written.
fn expandvars(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some((dollar, tail)) = rest.split_once('$') {
    out.push_str(dollar);
    if let Some(braced) = tail.strip_prefix('{') {
      let Some((name, remainder)) = braced.split_once('}') else {
        out.push_str("${");
        rest = tail.strip_prefix('{').unwrap_or_default();
        continue;
      };
      if is_env_name(name) {
        out.push_str(&std::env::var(name).unwrap_or_else(|_| format!("${{{name}}}")));
        rest = remainder;
      } else {
        out.push_str("${");
        rest = tail.strip_prefix('{').unwrap_or_default();
      }
    } else {
      let name: String = tail.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
      if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        out.push('$');
        rest = tail;
      } else {
        out.push_str(&std::env::var(&name).unwrap_or_else(|_| format!("${name}")));
        rest = &tail[name.len()..];
      }
    }
  }
  out.push_str(rest);
  out
}

fn is_env_name(name: &str) -> bool {
  !name.is_empty()
    && !name.starts_with(|c: char| c.is_ascii_digit())
    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn decode_utf8(bytes: &[u8]) -> Result<String, String> {
  std::str::from_utf8(bytes).map(str::to_string).map_err(|error| json_loads::utf8_error_text(bytes, &error))
}

// ---------------------------------------------------------------------------
// Error text: the OS error string and the JSON parser's message.
// ---------------------------------------------------------------------------

/// The error text of a chat log that could not be read: the OS error sentence with the failing path.
fn log_read_error(events_path: &Path, error: LogReadError) -> String {
  match error {
    LogReadError::Io(error) | LogReadError::Line { error, .. } => {
      charliebot_core::os_error::text(&error, &str_repr(&paths::display(events_path)))
    }
    LogReadError::Utf8(error) => json_loads::utf8_error_text(error.as_bytes(), &error.utf8_error()),
  }
}

/// The JSON parser's error message for one log line, from serde's message. Both scanners point at
/// the offending byte: serde's 1-based column becomes the message's column, and the message's
/// `char` index is the byte's character index. The EOF classes shift one column: this scanner
/// stops after consuming the whole input, serde reports the last byte it read.
fn json_error(line: &str, error: &serde_json::Error) -> String {
  let message = error.to_string();
  let Some((text, location)) = message.rsplit_once(" at line ") else { return message };
  let Some((line_text, column_text)) = location.split_once(" column ") else { return message };
  let (Ok(line_number), Ok(column)) = (line_text.parse::<usize>(), column_text.parse::<usize>()) else {
    return message;
  };
  if text.starts_with("EOF while parsing") {
    let char_index = char_index_of_byte(line, column);
    return format!("Expecting value: line {line_number} column {} (char {char_index})", column + 1);
  }
  if text == "expected ident" {
    // A word where a value belongs: serde consumed the literal's first byte, and this scanner
    // stops on it.
    let char_index = char_index_of_byte(line, column - 2);
    return format!("Expecting value: line {line_number} column {} (char {char_index})", column - 1);
  }
  let phrase = match text {
    "expected value" => "Expecting value",
    "trailing characters" => "Extra data",
    "key must be a string" => "Expecting property name enclosed in double quotes",
    "expected `:`" => "Expecting ':' delimiter",
    "expected `,` or `}`" | "expected `,` or `]`" => "Expecting ',' delimiter",
    "trailing comma" => "Illegal trailing comma before end of object",
    "invalid escape" => "Invalid \\escape",
    "control character (\\u0000-\\u001F) found while parsing a string" => "Invalid control character at",
    other => return format!("{other} at line {line_number} column {column}"),
  };
  let char_index = char_index_of_byte(line, column - 1);
  format!("{phrase}: line {line_number} column {column} (char {char_index})")
}

/// The character index of a byte offset (counting the chars a prefix of that length holds).
fn char_index_of_byte(text: &str, byte_index: usize) -> usize {
  text.char_indices().take_while(|(index, _)| *index < byte_index).count()
}

// ---------------------------------------------------------------------------
// ISO timestamps
// ---------------------------------------------------------------------------

/// An ISO 8601 timestamp, with `Z` read as `+00:00`, reduced to the instant the value names, in
/// microseconds since the epoch. The accepted grammar covers calendar dates; week dates and
/// ordinal dates are refused here.
fn parse_iso_moment(raw: &str) -> Result<i64, String> {
  let text = raw.replace('Z', "+00:00");
  let invalid = || format!("Invalid isoformat string: {}", str_repr(&text));
  let (year, month, day, mut rest) = parse_date(&text).ok_or_else(invalid)?;

  let mut hour = 0i64;
  let mut minute = 0i64;
  let mut second = 0i64;
  let mut micros = 0i64;
  if !rest.is_empty() {
    rest = rest.get(1..).ok_or_else(invalid)?; // one separator character of any kind
    (hour, rest) = take_two_digits(rest).ok_or_else(invalid)?;
    if let Some(after) = rest.strip_prefix(':') {
      (minute, rest) = take_two_digits(after).ok_or_else(invalid)?;
    } else if rest.len() >= 2 && rest.as_bytes()[..2].iter().all(|byte| byte.is_ascii_digit()) {
      (minute, rest) = take_two_digits(rest).ok_or_else(invalid)?;
    }
    let seconds = if let Some(after) = rest.strip_prefix(':') {
      take_two_digits(after).ok_or_else(invalid)?
    } else if rest.len() >= 2 && rest.as_bytes()[..2].iter().all(|byte| byte.is_ascii_digit()) {
      take_two_digits(rest).ok_or_else(invalid)?
    } else {
      (0, rest)
    };
    (second, rest) = seconds;
    if rest.starts_with(['.', ',']) {
      (micros, rest) = take_fraction(rest).ok_or_else(invalid)?;
    }
  }

  let mut aware = false;
  let mut offset_seconds = 0i64;
  if rest.starts_with(['+', '-']) {
    let sign: i64 = if rest.starts_with('+') { 1 } else { -1 };
    let (offset_hours, after_hours) = take_two_digits(&rest[1..]).ok_or_else(invalid)?;
    let (offset_minutes, after_minutes) = if let Some(after) = after_hours.strip_prefix(':') {
      take_two_digits(after).ok_or_else(invalid)?
    } else if after_hours.len() >= 2 && after_hours.as_bytes()[..2].iter().all(|byte| byte.is_ascii_digit()) {
      take_two_digits(after_hours).ok_or_else(invalid)?
    } else {
      (0, after_hours)
    };
    let (offset_secs, mut after_seconds) = if let Some(after) = after_minutes.strip_prefix(':') {
      take_two_digits(after).ok_or_else(invalid)?
    } else if after_minutes.len() >= 2 && after_minutes.as_bytes()[..2].iter().all(|byte| byte.is_ascii_digit()) {
      take_two_digits(after_minutes).ok_or_else(invalid)?
    } else {
      (0, after_minutes)
    };
    let mut offset_micros = 0i64;
    if after_seconds.starts_with(['.', ',']) {
      let fraction = take_fraction(after_seconds).ok_or_else(invalid)?;
      (offset_micros, after_seconds) = fraction;
    }
    if !after_seconds.is_empty() {
      return Err(invalid());
    }
    aware = true;
    offset_seconds = sign * (offset_hours * 3600 + offset_minutes * 60 + offset_secs);
    if offset_seconds.abs() >= 86400 {
      return Err(format!(
        "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24), not {}",
        timedelta_repr(offset_seconds, offset_micros.unsigned_abs() as u32)
      ));
    }
  } else if !rest.is_empty() {
    return Err(invalid());
  }

  if !(1..=9999).contains(&year) {
    return Err(format!("year {year} is out of range"));
  }
  if !(1..=12).contains(&month) {
    return Err(format!("month must be in 1..12, not {month}"));
  }
  let last_day = days_in_month(year, month);
  if !(1..=last_day).contains(&day) {
    return Err(format!("day {day} must be in range 1..{last_day} for month {month} in year {year}"));
  }
  if hour > 24 {
    return Err(format!("hour must be in 0..23, not {hour}"));
  }
  let mut day = day;
  if hour == 24 {
    if minute != 0 || second != 0 || micros != 0 {
      return Err("minute, second, and microsecond must be 0 when hour is 24".to_string());
    }
    day += 1;
  }
  if !(0..=59).contains(&minute) {
    return Err(format!("minute must be in 0..59, not {minute}"));
  }
  if !(0..=59).contains(&second) {
    return Err(format!("second must be in 0..59, not {second}"));
  }
  if !aware {
    return Err(format!("skill tool call timestamp is not timezone-aware: {raw}"));
  }
  let days = days_from_civil(year, month, day);
  Ok((days * 86400 + hour * 3600 + minute * 60 + second) * 1_000_000 + micros - offset_seconds * 1_000_000)
}

/// The calendar date and the text after it: `YYYY-MM-DD` or `YYYYMMDD`.
fn parse_date(text: &str) -> Option<(i64, i64, i64, &str)> {
  let bytes = text.as_bytes();
  if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' {
    let year = digits(&text[0..4])?;
    let month = digits(&text[5..7])?;
    let day = digits(&text[8..10])?;
    return Some((year, month, day, &text[10..]));
  }
  if bytes.len() >= 8 && bytes[..8].iter().all(u8::is_ascii_digit) {
    let compact = digits(&text[0..8])?;
    return Some((compact / 10000, (compact / 100) % 100, compact % 100, &text[8..]));
  }
  None
}

fn digits(text: &str) -> Option<i64> {
  if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  text.parse().ok()
}

fn take_two_digits(text: &str) -> Option<(i64, &str)> {
  if text.len() < 2 || !text.as_bytes()[..2].iter().all(u8::is_ascii_digit) {
    return None;
  }
  Some((text[..2].parse().ok()?, &text[2..]))
}

/// One `[.,]<digits>` fraction, truncated to microseconds.
fn take_fraction(text: &str) -> Option<(i64, &str)> {
  let digits_text = &text[1..];
  let end = digits_text.bytes().position(|byte| !byte.is_ascii_digit()).unwrap_or(digits_text.len());
  if end == 0 {
    return None;
  }
  let scaled = if end >= 6 {
    digits_text[..6].parse::<i64>().ok()?
  } else {
    format!("{}{}", &digits_text[..end], "0".repeat(6 - end)).parse::<i64>().ok()?
  };
  Some((scaled, &digits_text[end..]))
}

fn days_in_month(year: i64, month: i64) -> i64 {
  match month {
    1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
    4 | 6 | 9 | 11 => 30,
    _ => {
      if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
        29
      } else {
        28
      }
    }
  }
}

/// Howard Hinnant's `days_from_civil`: the count of days between 1970-01-01 and the date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
  let y = year - i64::from(month <= 2);
  let era = if y >= 0 { y } else { y - 399 } / 400;
  let year_of_era = y - era * 400;
  let month_shift = if month > 2 { month - 3 } else { month + 9 };
  let day_of_year = (153 * month_shift + 2) / 5 + day - 1;
  let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
  era * 146097 + day_of_era - 719468
}

/// The `datetime.timedelta(...)` text for the offset range error.
fn timedelta_repr(total_seconds: i64, micros: u32) -> String {
  let days = total_seconds.div_euclid(86400);
  let seconds = total_seconds.rem_euclid(86400);
  let mut parts: Vec<String> = Vec::new();
  if days != 0 {
    parts.push(format!("days={days}"));
  }
  if seconds != 0 {
    parts.push(format!("seconds={seconds}"));
  }
  if micros != 0 {
    parts.push(format!("microseconds={micros}"));
  }
  if parts.is_empty() {
    "datetime.timedelta(0)".to_string()
  } else {
    format!("datetime.timedelta({})", parts.join(", "))
  }
}

// ---------------------------------------------------------------------------
// The skill files and the part split
// ---------------------------------------------------------------------------

fn load_skill_texts(events_path: &Path, home: &Path, repo: &Path) -> Result<(Vec<SkillText>, Vec<String>), String> {
  let mut skills = Vec::new();
  let mut unresolved = Vec::new();
  for read in latest_skill_reads(events_path)? {
    let Some(path) = resolved_skill_path(&read, home, repo) else {
      unresolved.push(read.name);
      continue;
    };
    let text = read_text(&path)?;
    skills.push(SkillText { name: read.name, path, text });
  }
  Ok((skills, unresolved))
}

/// The named path first, then the skill's current file in the profile home and the repository.
fn resolved_skill_path(read: &SkillRead, home: &Path, repo: &Path) -> Option<PathBuf> {
  if let Some(named_path) = &read.named_path {
    let expanded = paths::expanduser(&expandvars(named_path));
    let mut named = PathBuf::from(expanded);
    if !named.is_absolute() {
      named = read.command_cwd.clone().unwrap_or_else(|| repo.to_path_buf()).join(named);
    }
    if is_file(&named) {
      return Some(named);
    }
  }
  for root in [home.join("skills"), repo.join("skills")] {
    let candidate = root.join(&read.name).join("SKILL.md");
    if is_file(&candidate) {
      return Some(candidate);
    }
  }
  None
}

/// Whether the path is a regular file: every error reads as "not a file".
fn is_file(path: &Path) -> bool {
  std::fs::metadata(path).map(|metadata| metadata.is_file()).unwrap_or(false)
}

fn read_text(path: &Path) -> Result<String, String> {
  let bytes =
    std::fs::read(path).map_err(|error| charliebot_core::os_error::text(&error, &str_repr(&paths::display(path))))?;
  let text = decode_utf8(&bytes)?;
  Ok(universal_newlines(&text))
}

fn skill_block(skill: &SkillText) -> String {
  let body = format!("Skill {} ({}), re-attached after compaction:\n{}", skill.name, skill.path.display(), skill.text);
  if body.ends_with('\n') { body } else { format!("{body}\n") }
}

/// Split text at the ten line boundaries, keeping each terminator.
fn splitlines_keepends(text: &str) -> Vec<&str> {
  let mut lines = Vec::new();
  let mut start = 0;
  let mut chars = text.char_indices().peekable();
  while let Some((index, character)) = chars.next() {
    let length = match character {
      '\r' if chars.peek() == Some(&(index + 1, '\n')) => {
        chars.next();
        2
      }
      '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}' => 1,
      _ => continue,
    };
    lines.push(&text[start..index + length]);
    start = index + length;
  }
  if start < text.len() {
    lines.push(&text[start..]);
  }
  lines
}

/// What a compaction restores before the skills: the memo sentence for a manager session, or a worker run's
/// task spec.
enum Restore {
  Memo,
  /// The run's pinned task spec, or `None` when the run pinned none.
  Task(Option<TaskSpec>),
}

/// A worker run's pinned task spec: its text, and the file it is read from.
struct TaskSpec {
  text: String,
  path: PathBuf,
}

impl Restore {
  /// The text the first part opens with. The skills follow it.
  fn lead(&self, skills: &[SkillText]) -> Result<String, String> {
    match self {
      Restore::Memo => Ok(format!("{SESSION_MEMO_NOTE}\n")),
      Restore::Task(None) => Ok(String::new()),
      Restore::Task(Some(spec)) => task_lead(spec, skills),
    }
  }
}

/// The run's task spec file, or `None` when the run pinned none.
fn read_task_spec(session_dir: &Path, run_id: &str) -> Result<Option<TaskSpec>, String> {
  let path = layout::runs_dir(session_dir).join(run_id).join(RUN_TASK_SPEC_NAME);
  let bytes = match std::fs::read(&path) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(charliebot_core::os_error::text(&error, &str_repr(&paths::display(&path)))),
  };
  Ok(Some(TaskSpec { text: decode_utf8(&bytes)?, path }))
}

/// The task lead: the header, then the first `restored` characters of the task spec, then the note line when
/// `restored` is short of the whole text. The lead keeps room for the name line of every skill, so a skill left
/// out still has its name in the output.
fn task_lead(spec: &TaskSpec, skills: &[SkillText]) -> Result<String, String> {
  let total = spec.text.chars().count();
  let all_skills: Vec<&SkillText> = skills.iter().collect();
  let packs = |restored: usize| partition(&task_text(spec, restored), &[], &all_skills).is_some();
  if packs(total) {
    return Ok(task_text(spec, total));
  }
  if !packs(0) {
    return Err("the task spec header and the skill names exceed the six parts".to_string());
  }
  // `packs(low)` holds and `packs(high)` does not; the search narrows the cut to the largest count that packs.
  let (mut low, mut high) = (0, total);
  while high - low > 1 {
    let middle = (low + high) / 2;
    if packs(middle) {
      low = middle;
    } else {
      high = middle;
    }
  }
  Ok(task_text(spec, low))
}

/// The header, the first `restored` characters of the task spec, and the note line when the text is cut.
fn task_text(spec: &TaskSpec, restored: usize) -> String {
  let total = spec.text.chars().count();
  let mut text = format!("{TASK_SPEC_HEADER}\n{}", spec.text.chars().take(restored).collect::<String>());
  if restored < total {
    text.push_str(&format!(
      "\nRestored {restored} of {total} characters of the task spec. The full text is at {}.\n",
      spec.path.display()
    ));
  } else if !text.ends_with('\n') {
    text.push('\n');
  }
  text
}

/// The lead text, then the skill lines, packed greedily into at most `MAX_PARTS` parts of at most `MAX_PART_CHARS`
/// characters. The lead splits at any character; the skill lines split only at line ends. `None` when a skill line
/// does not fit a part, or when the parts run out.
fn partition(lead: &str, skills: &[&SkillText], omitted: &[&SkillText]) -> Option<Vec<String>> {
  let names = if skills.is_empty() {
    "none".to_string()
  } else {
    skills.iter().map(|skill| format!("{} ({})", skill.name, skill.path.display())).collect::<Vec<_>>().join(", ")
  };
  let mut lines = Vec::new();
  if !skills.is_empty() || !omitted.is_empty() {
    lines.push(format!("Re-attached skills: {names}\n"));
    if !omitted.is_empty() {
      lines.push("Skills left out because they do not fit:\n".to_string());
      lines.extend(omitted.iter().map(|skill| format!("- {} ({})\n", skill.name, skill.path.display())));
    }
    for skill in skills {
      lines.extend(splitlines_keepends(&skill_block(skill)).into_iter().map(str::to_string));
    }
  }

  let mut slots = Slots::new();
  for character in lead.chars() {
    slots.push_char(character)?;
  }
  for line in &lines {
    slots.push_line(line)?;
  }
  Some(slots.parts)
}

/// The output parts being filled in order: each holds at most `MAX_PART_CHARS` characters, and at most `MAX_PARTS`
/// parts exist.
struct Slots {
  parts: Vec<String>,
  filled: usize,
}

impl Slots {
  fn new() -> Self {
    Self { parts: vec![String::new()], filled: 0 }
  }

  fn push_char(&mut self, character: char) -> Option<()> {
    if self.filled == MAX_PART_CHARS {
      self.open_next()?;
    }
    self.parts.last_mut().expect("parts is never empty").push(character);
    self.filled += 1;
    Some(())
  }

  fn push_line(&mut self, line: &str) -> Option<()> {
    let length = line.chars().count();
    if length > MAX_PART_CHARS {
      return None;
    }
    if self.filled + length > MAX_PART_CHARS {
      self.open_next()?;
    }
    self.parts.last_mut().expect("parts is never empty").push_str(line);
    self.filled += length;
    Some(())
  }

  fn open_next(&mut self) -> Option<()> {
    if self.parts.len() == MAX_PARTS {
      return None;
    }
    self.parts.push(String::new());
    self.filled = 0;
    Some(())
  }
}

/// The greedy fit, then the last included skills drop out until the remainder packs. The name and
/// path list overflowing the first part is a hard error.
/// The answer: the parts, the skills they re-attach, and the skills left out.
type ContextParts = (Vec<String>, Vec<SkillText>, Vec<SkillText>);

fn context_parts(restore: &Restore, skills: &[SkillText]) -> Result<ContextParts, String> {
  let lead = restore.lead(skills)?;
  let mut included: Vec<SkillText> = Vec::new();
  let mut omitted: Vec<SkillText> = Vec::new();
  for skill in skills {
    let mut trial: Vec<&SkillText> = included.iter().collect();
    trial.push(skill);
    if partition(&lead, &trial, &omitted.iter().collect::<Vec<_>>()).is_none() {
      omitted.push(skill.clone());
    } else {
      included.push(skill.clone());
    }
  }
  let mut parts = partition(&lead, &included.iter().collect::<Vec<_>>(), &omitted.iter().collect::<Vec<_>>());
  while parts.is_none() && !included.is_empty() {
    let last = included.pop().expect("included is not empty");
    omitted.push(last);
    parts = partition(&lead, &included.iter().collect::<Vec<_>>(), &omitted.iter().collect::<Vec<_>>());
  }
  let parts = parts.ok_or_else(|| "the skill name and path list exceeds the 9,000-character first part".to_string())?;
  Ok((parts, included, omitted))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The events one tool call carries.
  fn tool_event(timestamp: &str, name: &str, tool_input: Value) -> Value {
    json!({
      "type": "assistant",
      "timestamp": timestamp,
      "message": { "content": [{ "type": "tool_use", "name": name, "input": tool_input }] },
    })
  }

  fn write_events(path: &Path, events: &[Value]) {
    std::fs::create_dir_all(path.parent().expect("the events path has a parent")).unwrap();
    let text: String = events.iter().map(|event| format!("{}\n", serde_json::to_string(event).unwrap())).collect();
    std::fs::write(path, text).unwrap();
  }

  fn skill_text(name: &str, path: &Path, text: &str) -> SkillText {
    SkillText { name: name.to_string(), path: path.to_path_buf(), text: text.to_string() }
  }

  #[test]
  fn test_reads_bash_read_skill_and_split_path_inputs_in_latest_read_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    let repo = root.join("repo");
    let bash_old = root.join("first").join("skills").join("bash-read").join("SKILL.md");
    let bash_current = root.join("current").join("skills").join("bash-read").join("SKILL.md");
    let read_path = root.join("reader").join("skills").join("read-tool").join("SKILL.md");
    for directory in [bash_old.parent().unwrap(), bash_current.parent().unwrap(), read_path.parent().unwrap()] {
      std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(&bash_old, "old Bash version\n").unwrap();
    std::fs::write(&bash_current, "current Bash text\n").unwrap();
    std::fs::write(&read_path, "Read tool text\n").unwrap();

    let skill_tool = repo.join("skills").join("skill-tool").join("SKILL.md");
    let split_skill = repo.join("skills").join("split-path").join("SKILL.md");
    std::fs::create_dir_all(skill_tool.parent().unwrap()).unwrap();
    std::fs::create_dir_all(split_skill.parent().unwrap()).unwrap();
    std::fs::write(&skill_tool, "Skill tool text\n").unwrap();
    std::fs::write(&split_skill, "Split path text\n").unwrap();

    let events_path = &home.join("sessions").join("session-a").join("data").join("chat_events.jsonl");
    write_events(
      events_path,
      &[
        tool_event("2026-01-01T00:01:00Z", "Bash", json!({ "command": format!("cat {}", bash_old.display()) })),
        tool_event(
          "2026-01-01T00:02:00Z",
          "Bash",
          json!({ "command": format!("cd {} && cat skills/split-path/notes.md && echo SKILL.md", root.display()) }),
        ),
        tool_event("2026-01-01T00:03:00Z", "Read", json!({ "file_path": read_path.display().to_string(), "limit": 1 })),
        tool_event(
          "2026-01-01T00:04:00Z",
          "Bash",
          json!({ "command": format!("sed -n 2,4p {}", bash_current.display()) }),
        ),
        tool_event("2026-01-01T00:05:00Z", "Skill", json!({ "skill": "skill-tool" })),
        tool_event(
          "2026-01-01T00:06:00Z",
          "Read",
          json!({ "file_path": root.join("skills").join("not-a-skill").join("README.md").display().to_string() }),
        ),
        tool_event(
          "2026-01-01T00:07:00Z",
          "Bash",
          json!({ "command": format!("cat {}/skills/another/README.md", root.display()) }),
        ),
      ],
    );

    let (skills, unresolved) = load_skill_texts(events_path, &home, &repo).unwrap();

    assert!(unresolved.is_empty());
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, ["skill-tool", "bash-read", "read-tool", "split-path"]);
    assert_eq!(skills[0].text, "Skill tool text\n");
    assert_eq!(skills[1].path, bash_current);
    assert_eq!(skills[1].text, "current Bash text\n");
    assert_eq!(skills[2].text, "Read tool text\n");
    assert_eq!(skills[3].path, split_skill);
    assert_eq!(skills[3].text, "Split path text\n");
  }

  #[test]
  fn test_memo_only_prints_the_sentence_and_no_skill_text() {
    let rendered = json_out::render(&hook_json(SESSION_MEMO_NOTE), None);
    let parsed: Value = serde_json::from_str(&rendered).unwrap();

    assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], claude_launch::SESSION_START_HOOK_EVENT);
    assert_eq!(parsed["hookSpecificOutput"]["additionalContext"], SESSION_MEMO_NOTE);
    assert_eq!(parsed["hookSpecificOutput"].as_object().unwrap().len(), 2);
    assert!(!rendered.contains("Re-attached skills"), "{rendered}");
  }

  #[test]
  fn test_first_part_opens_with_the_memo_sentence_with_a_skill() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("skills").join("one").join("SKILL.md");
    let skills = [skill_text("one", &path, "Skill text\n")];

    let (parts, _included, _omitted) = context_parts(&Restore::Memo, &skills).unwrap();

    assert!(parts[0].starts_with(&format!("{SESSION_MEMO_NOTE}\n")), "{}", parts[0]);
    assert!(parts[0].contains("Re-attached skills: one ("));
    assert_eq!(parts[0].matches(SESSION_MEMO_NOTE).count(), 1);
  }

  #[test]
  fn test_first_part_opens_with_the_memo_sentence_without_a_skill() {
    let (parts, included, omitted) = context_parts(&Restore::Memo, &[]).unwrap();

    assert_eq!(parts, [format!("{SESSION_MEMO_NOTE}\n")]);
    assert!(included.is_empty());
    assert!(omitted.is_empty());
  }

  /// A worker's first part opens with the task header and carries the whole 2,583-character task spec in one part,
  /// ahead of the skills.
  #[test]
  fn test_worker_first_part_opens_with_the_task_spec_before_the_skills() {
    let temp = tempfile::tempdir().unwrap();
    let spec_text = format!("{{\"goal\":\"{}\"}}", "g".repeat(2583 - 11));
    let spec = TaskSpec { text: spec_text.clone(), path: temp.path().join("task_spec.md") };
    let skill = skill_text("one", &temp.path().join("skills").join("one").join("SKILL.md"), "Skill text\n");

    let (parts, included, omitted) = context_parts(&Restore::Task(Some(spec)), &[skill]).unwrap();

    assert_eq!(parts.len(), 1, "{parts:?}");
    assert!(parts[0].starts_with(&format!("{TASK_SPEC_HEADER}\n{spec_text}\n")), "{}", parts[0]);
    assert!(!parts[0].contains("Restored "), "{}", parts[0]);
    assert_eq!(included.len(), 1);
    assert!(omitted.is_empty());
  }

  /// A 60,000-character task spec fills all six parts, and the last part names the restored count, the total count
  /// and the file's path. The restored count is the characters of the spec in the parts.
  #[test]
  fn test_worker_task_spec_past_six_parts_is_cut_and_the_last_part_names_the_count_and_path() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("task_spec.md");
    // A character that the header, the note and the temporary path never hold, so counting it counts the spec.
    let spec = TaskSpec { text: "é".repeat(60_000), path: path.clone() };

    let (parts, _included, _omitted) = context_parts(&Restore::Task(Some(spec)), &[]).unwrap();

    assert_eq!(parts.len(), MAX_PARTS);
    assert!(parts.iter().all(|part| part.chars().count() <= MAX_PART_CHARS));
    let restored = parts.concat().matches('é').count();
    assert!(restored > 50_000 && restored < 60_000, "restored {restored}");
    let last = &parts[MAX_PARTS - 1];
    assert!(last.contains(&restored.to_string()) && last.contains("60000"), "{last}");
    assert!(last.contains(&path.display().to_string()), "{last}");
    assert!(!parts[0].contains("60000"), "{}", parts[0]);
  }

  #[test]
  fn test_parts_obey_character_limit_and_split_only_at_line_ends() {
    let temp = tempfile::tempdir().unwrap();
    let skill = skill_text(
      "long-lines",
      &temp.path().join("skills").join("long-lines").join("SKILL.md"),
      &format!("{}\n{}\n", "A".repeat(4499), "B".repeat(4499)),
    );

    let (parts, included, omitted) = context_parts(&Restore::Memo, &[skill]).unwrap();

    assert_eq!(parts.len(), 2);
    assert!(parts.iter().all(|part| part.chars().count() <= 9000));
    assert!(parts.iter().all(|part| part.ends_with('\n')));
    assert!(parts[0].contains(&format!("{}\n", "A".repeat(4499))));
    assert!(parts[1].contains(&format!("{}\n", "B".repeat(4499))));
    assert_eq!(included.iter().map(|skill| skill.name.as_str()).collect::<Vec<_>>(), ["long-lines"]);
    assert!(omitted.is_empty());
  }

  #[test]
  fn test_overflow_skills_are_listed_and_not_partially_attached() {
    let temp = tempfile::tempdir().unwrap();
    let path = |name: &str| temp.path().join("skills").join(name).join("SKILL.md");
    let too_large = skill_text("too-large", &path("too-large"), &"z".repeat(9001));
    let skills: Vec<SkillText> = std::iter::once(too_large)
      .chain((0..8).map(|index| {
        skill_text(
          &format!("skill-{index}"),
          &path(&format!("skill-{index}")),
          &format!("{}\n", index.to_string().repeat(8900)),
        )
      }))
      .collect();

    let (parts, included, omitted) = context_parts(&Restore::Memo, &skills).unwrap();
    let rendered = parts.join("");

    assert!(parts.len() <= 6);
    assert!(parts.iter().all(|part| part.chars().count() <= 9000));
    assert!(!omitted.is_empty());
    assert!(parts[0].contains("Skills left out because they do not fit:"));
    for skill in &omitted {
      assert!(parts[0].contains(&format!("- {} ({})\n", skill.name, skill.path.display())));
      assert!(!rendered.contains(&format!("Skill {} (", skill.name)));
    }
    for skill in &included {
      assert!(rendered.contains(&format!(
        "Skill {} ({}), re-attached after compaction:",
        skill.name,
        skill.path.display()
      )));
    }
  }

  #[test]
  fn test_pattern_location_is_no_skill_read() {
    let locations = [
      "skills/*/SKILL.md",
      "skills/{a,b}/SKILL.md",
      "skills/$s/SKILL.md",
      "skills/google-*/SKILL.md",
      "skills/plan-approval, SKILL.md",
    ];
    for location in locations {
      let temp = tempfile::tempdir().unwrap();
      let home = temp.path().join("home");
      let repo = temp.path().join("repo");
      let events_path = &home.join("sessions").join("session-pattern").join("data").join("chat_events.jsonl");
      write_events(
        events_path,
        &[
          tool_event("2026-01-01T00:00:00Z", "Bash", json!({ "command": format!("cat {location}") })),
          tool_event("2026-01-01T00:01:00Z", "Read", json!({ "file_path": format!("/repo/{location}") })),
        ],
      );

      let (skills, unresolved) = load_skill_texts(events_path, &home, &repo).unwrap();

      assert!(skills.is_empty(), "{location}");
      assert!(unresolved.is_empty(), "{location}");
    }
  }

  #[test]
  fn test_plain_skill_name_with_dot_underscore_or_digit_is_read() {
    for name in ["skill.v2", "skill_v2", "skill2"] {
      let temp = tempfile::tempdir().unwrap();
      let home = temp.path().join("home");
      let repo = temp.path().join("repo");
      let skill = repo.join("skills").join(name).join("SKILL.md");
      std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
      std::fs::write(&skill, "Plain name text\n").unwrap();
      let events_path = &home.join("sessions").join("session-plain").join("data").join("chat_events.jsonl");
      write_events(
        events_path,
        &[tool_event("2026-01-01T00:00:00Z", "Read", json!({ "file_path": skill.display().to_string() }))],
      );

      let (skills, unresolved) = load_skill_texts(events_path, &home, &repo).unwrap();

      assert!(unresolved.is_empty());
      let read: Vec<(String, String)> = skills.iter().map(|skill| (skill.name.clone(), skill.text.clone())).collect();
      assert_eq!(read, [(name.to_string(), "Plain name text\n".to_string())]);
    }
  }
}
