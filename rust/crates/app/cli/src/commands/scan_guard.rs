//! The `PreToolUse` hook for unbounded scans in Bash commands, for Claude Code and codex. A refused scan is one of six
//! shell-level forms: one process per session directory; a search from `/`, `/home` or the home directory that neither
//! limits depth nor prunes the session store; a recursive search of the session store or a parent of it whose file-name
//! filter reaches run logs; a find that walks the store or a parent of it with no depth limit, alone or with a command
//! on each file; or one process that reads the run logs of every session through a glob. An argument's quoting does not
//! change its meaning, so a quoted store path counts as its bare form. Script code inside quotes or heredoc bodies is
//! not shell, and a relative path resolves against the hook's cwd and the latest `cd`.

mod shell;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::OnceLock;

use charliebot_claude_code::claude_launch;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config::SESSIONS_DIR_NAME;
use charliebot_core::home::CHARLIEBOT_HOME_ENV;
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_store::layout::DATA_DIR_NAME;
use charliebot_store::run_records::{RAW_LOG_NAME, RUN_EVENTS_NAME};
use fancy_regex::{Captures, Regex};
use serde_json::{Value, json};
use shell::{parse_error, resolve_paths, shell_text, strip_quotes};

/// The profile-home directory name under the user's home.
const CHARLIEBOT_DIR: &str = ".charliebot";

/// A recursive `grep`: `-r`, `-R` or `--recursive` among its options, alone or grouped with other flags.
const RECURSIVE_GREP: &str = r"grep\b(?=[^|;&\n]*\s(?:--recursive|-[a-zA-Z]*[rR][a-zA-Z]*)(?=\s|$))";

/// The file names a run directory holds that a glob can reach as run logs, and examples of them for name matching.
const RUN_LOG_EXAMPLES: [&str; 4] = [RUN_EVENTS_NAME, RAW_LOG_NAME, "agent.raw.ndjson.1", "agent.stderr.log"];

/// The commands that read a file's text, for the glob rule: a glob given to one of them reads every file it names.
const READERS: &[&str] = &[
  "grep", "egrep", "fgrep", "rg", "ag", "ack", "cat", "head", "tail", "jq", "sed", "awk", "less", "more", "wc", "sort",
  "uniq", "cut", "tr", "xargs", "python3", "python", "node", "zcat", "strings", "diff", "cmp", "nl", "tac", "od", "xxd",
  "sha256sum", "md5sum",
];

/// The wrappers a reader may follow, which the glob rule skips.
const WRAPPERS: &[&str] = &["sudo", "timeout", "nice", "ionice", "time", "env"];

const PER_FILE_PROCESS_REASON: &str = "This command starts one process per session directory. Use `charliebot session tree --root <id> --include-archived --limit 500` for a session's children, or read all the files in one process.";
const ROOTED_SEARCH_REASON: &str = "This search runs from `/`, `/home` or the home directory with no depth limit. Search the directories that can hold the answer, add `-maxdepth`, or prune the session store.";
const RUN_LOG_SEARCH_REASON: &str = "This recursive search reaches the session store's run logs. Filter with `-g 'chat_events*.jsonl'`, `-g metadata.json` or `-g '*.md'`, or search a more specific directory.";
const FIND_WALK_REASON: &str = "This find walks the session store or a parent of it with no depth limit. Add `-maxdepth`, prune the session store with `-path ... -prune`, or search one session's directory.";
const FIND_READ_REASON: &str = "This find runs a command on every file under the session store or a parent of it. Use `charliebot session tree --root <id> --include-archived --limit 500` for a session's children, or name one session's directory.";
const RUN_LOG_GLOB_REASON: &str = "This command reads every session's run logs through a glob. Name one session's run directory, such as `~/.charliebot/sessions/<id>/data/runs/<run>/events.jsonl`, or read `metadata.json`.";

/// The unbounded scan form a Bash command holds; each refusal names the form's bounded replacement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScanForm {
  PerFileProcess,
  RootedSearch,
  RunLogSearch,
  FindRead,
  FindWalk,
  RunLogGlob,
}

impl ScanForm {
  fn reason(self) -> &'static str {
    match self {
      ScanForm::PerFileProcess => PER_FILE_PROCESS_REASON,
      ScanForm::RootedSearch => ROOTED_SEARCH_REASON,
      ScanForm::RunLogSearch => RUN_LOG_SEARCH_REASON,
      ScanForm::FindRead => FIND_READ_REASON,
      ScanForm::FindWalk => FIND_WALK_REASON,
      ScanForm::RunLogGlob => RUN_LOG_GLOB_REASON,
    }
  }
}

/// Read one hook event and emit a deny decision when its Bash command holds an unbounded scan.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  if !args.is_empty() {
    return Err(CliExit::usage("scan-guard takes no arguments"));
  }
  run(std::io::stdin().lock(), &mut std::io::stdout().lock())
}

fn run<R: Read, W: Write>(mut input: R, output: &mut W) -> Result<(), CliExit> {
  let mut input_text = String::new();
  input
    .read_to_string(&mut input_text)
    .map_err(|error| CliExit::error(format!("could not read hook input: {error}")))?;
  let event: Value =
    serde_json::from_str(&input_text).map_err(|error| CliExit::error(format!("invalid hook JSON: {error}")))?;
  if event.get("tool_name").and_then(Value::as_str) != Some("Bash") {
    return Ok(());
  }
  let command = event
    .pointer("/tool_input/command")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::error("Bash hook input must include a string tool_input.command"))?;
  let cwd = event
    .get("cwd")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::error("Bash hook input must include a string cwd"))?;
  let forms = scan_forms(command, cwd).map_err(CliExit::error)?;
  if let Some(form) = forms.first() {
    serde_json::to_writer(&mut *output, &deny_decision(form.reason()))
      .map_err(|error| CliExit::error(format!("could not write hook output: {error}")))?;
    output.write_all(b"\n").map_err(|error| CliExit::error(format!("could not write hook output: {error}")))?;
  }
  Ok(())
}

fn deny_decision(reason: &str) -> Value {
  json!({
    "hookSpecificOutput": {
      "hookEventName": claude_launch::PRE_TOOL_USE_HOOK_EVENT,
      "permissionDecision": "deny",
      "permissionDecisionReason": reason
    }
  })
}

/// The unbounded scan forms one command holds, in refusal order. The rules read the command as written and again with
/// its relative paths resolved against the hook's cwd, so `cd .. && find .` is the same search as its absolute form.
fn scan_forms(command: &str, cwd: &str) -> Result<Vec<ScanForm>, String> {
  let shell = shell_text(command);
  let resolved = resolve_paths(&shell, cwd, store_root_pattern()?)?;
  let mut forms = search_forms(&shell)?;
  for form in search_forms(&resolved)? {
    if !forms.contains(&form) {
      forms.push(form);
    }
  }
  Ok(forms)
}

fn search_forms(shell: &str) -> Result<Vec<ScanForm>, String> {
  let mut forms = Vec::new();
  if per_file_process_pattern()?.is_match(shell).map_err(parse_error)? {
    forms.push(ScanForm::PerFileProcess);
  }
  for captures in rooted_search_pattern()?.captures_iter(shell) {
    let captures = captures.map_err(parse_error)?;
    let around = groups_text(&captures);
    let prunes_store = around.contains("-prune") && around.contains(SESSIONS_DIR_NAME);
    let bounded_depth = depth_limited(&around)?;
    if !bounded_depth && !prunes_store {
      forms.push(ScanForm::RootedSearch);
      break;
    }
  }
  for captures in run_log_search_pattern()?.captures_iter(shell) {
    let captures = captures.map_err(parse_error)?;
    let globs = glob_names(&groups_text(&captures))?;
    if globs.is_empty() || globs.iter().any(|glob| is_run_log_glob(glob)) {
      forms.push(ScanForm::RunLogSearch);
      break;
    }
  }
  let mut walks_store = false;
  let mut runs_per_file = false;
  for segment in find_segments(shell)? {
    if walks_store_root(&segment)? {
      walks_store = true;
      runs_per_file |= per_file_command_pattern().is_match(&segment.region).map_err(parse_error)?;
    }
  }
  if runs_per_file {
    forms.push(ScanForm::FindRead);
  }
  if walks_store {
    forms.push(ScanForm::FindWalk);
  }
  if reads_run_logs_by_glob(shell)? {
    forms.push(ScanForm::RunLogGlob);
  }
  Ok(forms)
}

fn groups_text(captures: &Captures<'_>) -> String {
  [1, 2].into_iter().filter_map(|index| captures.get(index)).map(|group| group.as_str()).collect()
}

fn glob_names(text: &str) -> Result<Vec<String>, String> {
  glob_pattern()
    .captures_iter(text)
    .map(|captures| {
      Ok(captures.map_err(parse_error)?.get(1).expect("glob pattern has a name group").as_str().to_string())
    })
    .collect()
}

/// Whether a file-name filter reaches run logs: one of the run-log names, or a pattern that matches one of their
/// examples.
fn is_run_log_glob(name: &str) -> bool {
  run_log_globs().contains(&name) || RUN_LOG_EXAMPLES.iter().any(|example| wildcard_matches(name, example))
}

fn run_log_globs() -> [&'static str; 9] {
  ["*", "**", "*.jsonl", "*.ndjson", "*.json", RUN_EVENTS_NAME, RAW_LOG_NAME, "agent.raw*", "*.log"]
}

/// Whether a shell wildcard pattern matches a whole name: `*` matches any run of characters and `?` one character.
fn wildcard_matches(pattern: &str, name: &str) -> bool {
  let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
  let (mut pattern_at, mut name_at) = (0, 0);
  let mut last_star = None;
  let mut star_name_at = 0;
  while name_at < name.len() {
    if pattern_at < pattern.len() && (pattern[pattern_at] == b'?' || pattern[pattern_at] == name[name_at]) {
      pattern_at += 1;
      name_at += 1;
    } else if pattern_at < pattern.len() && pattern[pattern_at] == b'*' {
      last_star = Some(pattern_at);
      star_name_at = name_at;
      pattern_at += 1;
    } else if let Some(star) = last_star {
      pattern_at = star + 1;
      star_name_at += 1;
      name_at = star_name_at;
    } else {
      return false;
    }
  }
  pattern[pattern_at..].iter().all(|&byte| byte == b'*')
}

/// A `find` command: the roots it starts from, its arguments up to a pipe, and its region up to the command separator.
struct FindSegment {
  roots: Vec<String>,
  args: String,
  region: String,
}

fn find_segments(shell: &str) -> Result<Vec<FindSegment>, String> {
  let mut segments = Vec::new();
  for word in find_word_pattern().find_iter(shell) {
    let word = word.map_err(parse_error)?;
    let rest = &shell[word.end()..];
    let region = find_region_pattern().find(rest).map_err(parse_error)?.map_or("", |region| region.as_str());
    let args = region.split(['|', '&']).next().unwrap_or("");
    let mut roots = Vec::new();
    for argument in args.split_whitespace() {
      if argument.starts_with(['-', '(', '!']) || argument.starts_with("\\(") || argument.starts_with("\\!") {
        break;
      }
      roots.push(strip_quotes(argument));
    }
    segments.push(FindSegment { roots, args: args.to_string(), region: region.to_string() });
  }
  Ok(segments)
}

/// A find that walks the session store or a parent of it: a store root, with no `-maxdepth` and no prune of `sessions`.
fn walks_store_root(segment: &FindSegment) -> Result<bool, String> {
  let mut names_store = false;
  for root in &segment.roots {
    if store_root_pattern()?.is_match(root.trim_end_matches('/')).map_err(parse_error)? {
      names_store = true;
      break;
    }
  }
  let prunes_store = segment.args.contains("-prune") && segment.args.contains(SESSIONS_DIR_NAME);
  Ok(names_store && !segment.args.contains("-maxdepth") && !prunes_store)
}

/// Whether a reader command's glob names run logs: one process that reads the run logs of every session.
fn reads_run_logs_by_glob(shell: &str) -> Result<bool, String> {
  let text = prose_pattern().replace_all(shell, |captures: &Captures<'_>| {
    let whole = captures.get(0).expect("group 0 is the whole match").as_str();
    let inner = captures.get(1).map_or("", |inner| inner.as_str());
    if inner.contains(|character: char| character.is_whitespace() || ";&|()<>".contains(character)) {
      " ".to_string()
    } else {
      whole.to_string()
    }
  });
  for segment in command_segment_pattern().find_iter(&text) {
    let segment = segment.map_err(parse_error)?;
    if !is_reader_segment(segment.as_str()) {
      continue;
    }
    let reaches = segment
      .as_str()
      .split(|character: char| character.is_whitespace() || ";&|()<>`".contains(character))
      .filter(|token| !token.is_empty())
      .any(|token| reaches_run_logs(&strip_quotes(token)));
    if reaches {
      return Ok(true);
    }
  }
  Ok(false)
}

/// A command segment whose first word, after its wrappers and options, is a reader.
fn is_reader_segment(segment: &str) -> bool {
  let words: Vec<String> = segment.split_whitespace().map(strip_quotes).collect();
  match words.iter().position(|word| !(WRAPPERS.contains(&word.as_str()) || is_option_or_count(word))) {
    Some(index) => READERS.contains(&words[index].as_str()),
    None => false,
  }
}

/// An option (`-n`, `--glob`) or a count with an optional unit (`5`, `10s`), which a wrapper may take before its
/// command.
fn is_option_or_count(word: &str) -> bool {
  if let Some(name) = word.strip_prefix("--").or_else(|| word.strip_prefix('-')) {
    return !name.is_empty() && name.chars().all(|character| character.is_alphanumeric() || character == '_');
  }
  let digits = word.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(word);
  !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit())
}

/// Whether one glob token reaches the run logs of more than one session. A session wildcard is a path component with a
/// wildcard that is not the last one. Under `data/runs/`, the run id must be a wildcard too, and the token reaches run
/// logs when it names the run directory itself or a run-log file, but not `metadata.json`. A literal run id is one
/// session's run, a single lookup. Elsewhere under a session wildcard, a name that is a run-log name counts.
fn reaches_run_logs(token: &str) -> bool {
  if token.starts_with('!') {
    return false;
  }
  let parts: Vec<&str> = token.split('/').collect();
  for index in 0..parts.len().saturating_sub(1) {
    if !is_wild(parts[index]) {
      continue;
    }
    if parts.get(index + 1..index + 3) == Some(&["data", "runs"][..]) {
      let rest = &parts[index + 3..];
      let run_directory_glob = rest.first().is_some_and(|first| is_wild(first));
      let names_run_log = rest.len() == 1 || rest.last().is_some_and(|last| is_run_log_glob(last));
      if run_directory_glob && rest.last() != Some(&METADATA_FILE_NAME) && names_run_log {
        return true;
      }
    } else if is_run_log_glob(parts[parts.len() - 1]) {
      return true;
    }
  }
  false
}

fn is_wild(component: &str) -> bool {
  component.contains(['*', '?', '['])
}

/// The session store: `~/.charliebot/sessions`, the absolute home's, or `$CHARLIEBOT_HOME`'s.
fn store_token() -> Result<String, String> {
  Ok(format!(
    concat!(
      r"(?:",
      r"(?:~|\$HOME|{home})/{charliebot_dir}/{sessions}",
      r"|\$\{{?{charliebot_home}\}}?/{sessions}",
      r")"
    ),
    home = home_token()?,
    charliebot_dir = CHARLIEBOT_DIR,
    charliebot_home = regex::escape(CHARLIEBOT_HOME_ENV),
    sessions = regex::escape(SESSIONS_DIR_NAME),
  ))
}

fn home_token() -> Result<String, String> {
  match std::env::var("HOME") {
    Ok(home) => Ok(regex::escape(&home)),
    Err(error) => Err(format!("scan command parsing failed: the patterns need HOME: {error}")),
  }
}

/// The session store or a parent of it: `~/.charliebot` and `$CHARLIEBOT_HOME`, each with or without `sessions`.
fn store_or_parent_token() -> Result<String, String> {
  Ok(format!(
    concat!(
      r"(?:",
      r"(?:~|\$HOME|{home})/{charliebot_dir}(?:/{sessions})?",
      r"|\$\{{?{charliebot_home}\}}?(?:/{sessions})?",
      r")"
    ),
    home = home_token()?,
    charliebot_dir = CHARLIEBOT_DIR,
    charliebot_home = regex::escape(CHARLIEBOT_HOME_ENV),
    sessions = regex::escape(SESSIONS_DIR_NAME),
  ))
}

/// An enumeration over every session directory: a store glob, `*/metadata.json` or `*/data/...` after a `cd` into the
/// store, or `$(ls|find ... <store>)`.
fn enumeration_token(store: &str) -> String {
  format!(
    concat!(r"(?:", r"{store}/\*", r"|(?<![\w/])\*/(?:{metadata}|{data}/)", r"|\$\((?:ls|find)\s[^)]*{store}", r")"),
    store = store,
    metadata = regex::escape(METADATA_FILE_NAME),
    data = regex::escape(DATA_DIR_NAME),
  )
}

/// (a) One process per session directory at the shell level: a `for` loop over an enumeration, an enumeration piped to
/// `while read`, `xargs -n1` after a store path, or `find <store> -exec`.
fn per_file_process_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      let store = store_token()?;
      let enumeration = enumeration_token(&store);
      Regex::new(&format!(
        concat!(
          r"\bfor\s+\w+\s+in\s+[^;\n]*?(?:{enumeration})[^;\n]*;\s*do\b",
          r"|(?:{enumeration})[^;\n]*\|\s*while\s+read\b",
          r"|{store}[^|;\n]*\|\s*xargs\s+(?:-\w+\s+)*-n\s*1\b",
          r"|\bfind\s+{store}\S*\s[^|;\n]*-exec\b[^;\n]*;"
        ),
        enumeration = enumeration,
        store = store
      ))
      .map_err(parse_error)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// (b) A search from `/`, `/home` or the home directory itself, with its arguments on both sides.
fn rooted_search_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      Regex::new(&format!(
        concat!(
          r"\b(?:find|rg|{recursive_grep}|fd)\b([^|;&\n]*?)",
          r"\s(?:/home/?|/|~|~/|\$HOME|{home}/?)(?=\s|$|\))([^|;&\n]*)"
        ),
        recursive_grep = RECURSIVE_GREP,
        home = home_token()?
      ))
      .map_err(parse_error)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Whether the search bounds its depth: find's `-maxdepth`, or rg's and fd's `--max-depth` (spaced or with `=`) and
/// `-d N`. grep's `-d` takes an action word, not a depth, so the short flag needs a number.
fn depth_limited(search_text: &str) -> Result<bool, String> {
  if search_text.contains("-maxdepth") {
    return Ok(true);
  }
  depth_flag_pattern()?.is_match(search_text).map_err(parse_error)
}

fn depth_flag_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| Regex::new(concat!(r"(?<![\w-])-d\s*\d", r"|--max-depth(?:\s+|=)\d")).map_err(parse_error))
    .as_ref()
    .map_err(Clone::clone)
}

/// (c) A recursive search of the store or a parent of it, with its arguments on both sides.
fn run_log_search_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      Regex::new(&format!(
        concat!(r"\b(?:rg|{recursive_grep})\b([^|;&\n]*?)", r#"\s\"?(?:{store_or_parent})/?\"?(?=\s|$|\))([^|;&\n]*)"#),
        recursive_grep = RECURSIVE_GREP,
        store_or_parent = store_or_parent_token()?
      ))
      .map_err(parse_error)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The session store or a parent of it, matched in full: the form a resolved path takes.
fn store_root_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| Regex::new(&format!("^(?:{})$", store_or_parent_token()?)).map_err(parse_error))
    .as_ref()
    .map_err(Clone::clone)
}

/// The `find` command word, and the text after it up to a command separator.
fn find_word_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"\bfind\b").expect("find pattern is valid"))
}

fn find_region_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"^(?:[^;&\n\\]|\\[^;\n])*").expect("find region pattern is valid"))
}

/// A `find` that runs a command on each file it finds: `-exec`, or a pipe into `xargs`.
fn per_file_command_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"-exec\b|\|\s*xargs\b").expect("per-file command pattern is valid"))
}

/// A double-quoted string that is prose: holds a space or a shell separator, so it is not one argument.
fn prose_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r#"(?s)"((?:[^"\\]|\\.)*)""#).expect("prose pattern is valid"))
}

/// A command segment: the text between shell separators.
fn command_segment_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"[^;&|\n()]+").expect("command segment pattern is valid"))
}

/// A file-name filter with its value: an exclusion (`-g '!...'`) matches none, so it names no glob.
fn glob_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| {
    Regex::new(r#"(?:-g|--glob|--include)[ =]['"]?(?!!)([^'" ]+)"#).expect("glob pattern is valid")
  })
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;

  use super::*;

  fn run_input(input: &str) -> (Result<(), CliExit>, Vec<u8>) {
    let mut output = Vec::new();
    let result = run(Cursor::new(input.as_bytes()), &mut output);
    (result, output)
  }

  fn hook_input(command: &str) -> String {
    serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": home()}).to_string()
  }

  fn home() -> String {
    std::env::var("HOME").expect("the test cases name the home through HOME")
  }

  #[test]
  fn scan_form_cases_match_the_replay_table() {
    let denied = [
      ScanForm::PerFileProcess,
      ScanForm::RootedSearch,
      ScanForm::RunLogSearch,
      ScanForm::PerFileProcess,
      ScanForm::PerFileProcess,
      ScanForm::PerFileProcess,
      ScanForm::RunLogSearch,
      ScanForm::RunLogSearch,
      ScanForm::PerFileProcess,
      ScanForm::RunLogSearch,
      ScanForm::PerFileProcess,
    ];
    let commands: [&str; 11] = [
      // The replay's first real command: a metadata loop after a `cd` into the store.
      r#"cd ~/.charliebot/sessions && for f in */metadata.json; do python3 -I -c "import json; print(json.load(open('$f')))"; done"#,
      "find / -path /proc -prune -o -type d -name dataviz -print 2>/dev/null | head -3",
      r#"rg -o 'ur-[A-Za-z0-9._-]+' "$CHARLIEBOT_HOME/sessions" ~/.charlie-code/sessions 2>/dev/null | sort -u"#,
      // A run-record loop whose python body is a heredoc, not shell.
      "cd ~/.charliebot/sessions && for m in */data/runs/*/metadata.json; do python3 - \"$m\" <<'EOF2'\nimport json\nd = json.load(open(\"$m\"))\nprint(d)\nEOF2\ndone",
      r#"for d in $(ls ~/.charliebot/sessions); do python3 -I -c 'print(1)' $d/metadata.json; done"#,
      "find ~/.charliebot/sessions -name metadata.json | xargs -n1 python3 -I x.py",
      r"rg -l X ~/.charliebot/sessions -g '*.jsonl'",
      r"rg -l X ~/.charliebot/sessions -g 'agent.raw.ndjson'",
      r"grep -l 520ad103 ~/.charliebot/sessions/*/metadata.json | while read f; do stat $f; done",
      // The absolute home form, spelled through the test home.
      &format!("grep -rl '39828874' {}/.charliebot/ 2>/dev/null", home()),
      // The same loop shape with a different body still refuses.
      r#"cd ~/.charliebot/sessions && for f in */metadata.json; do python3 -I -c "import json,sys; print(json.load(open('$f')).get('title'))"; done"#,
    ];
    for (command, expected) in commands.iter().zip(denied) {
      let forms = scan_forms(command, &home()).unwrap();
      assert_eq!(forms.first(), Some(&expected), "{command}");
    }
  }

  #[test]
  fn bounded_lookup_cases_hold_no_scan_form() {
    let commands: [&str; 10] = [
      "charliebot session tree --root 28f1df1f --include-archived --limit 500",
      r"rg -l -F X ~/.charliebot/sessions -g 'chat_events*.jsonl'",
      r"rg -l -F X ~/.charliebot/sessions -g '*.md'",
      "find / -maxdepth 4 -name X",
      &format!(r"find / \( -path /proc -o -path {}/.charliebot/sessions \) -prune -o -name X -print", home()),
      r"ls -d ~/.charliebot/sessions/92b2*",
      "rg -n X ~/workspace/charlie-bot/rust",
      r"for p in 92b28639 c8eb085f; do ls -d ~/.charliebot/sessions/${p}*/; done",
      &format!(
        r#"python3 -I -c 'import glob,json; [print(f) for f in glob.glob("{}/.charliebot/sessions/*/metadata.json") if "x" in open(f).read()]'"#,
        home()
      ),
      r"grep -l X ~/.charliebot/sessions/*/metadata.json",
    ];
    for command in commands {
      assert!(scan_forms(command, &home()).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn recursive_grep_and_unpruned_home_search_are_refused() {
    let denied = [
      "grep -n -r X ~/.charliebot/sessions",
      "grep -R X ~/.charliebot/sessions",
      "grep --recursive X ~/.charliebot/sessions",
      r#"find ~ -path "*/sessions/*" -name metadata.json"#,
      "rg -l X / ~/.charliebot/sessions -g metadata.json",
    ];
    for command in denied {
      assert!(!scan_forms(command, &home()).unwrap().is_empty(), "{command}");
    }
    assert!(scan_forms("grep -n -r X ~/workspace/charlie-bot/rust", &home()).unwrap().is_empty());
  }

  #[test]
  fn quoted_store_paths_mean_the_same_thing_bare() {
    // A single-quoted store path is the same search root as its bare form, and its run-log glob still refuses.
    assert_eq!(
      scan_forms(&format!(r"rg -l X '{}/.charliebot/sessions' -g '*.jsonl'", home()), &home()).unwrap(),
      vec![ScanForm::RunLogSearch]
    );
    // A single-quoted prune path keeps the prune it names.
    assert!(scan_forms(r"find ~ -path '*/.charliebot/sessions' -prune -o -name X -print", &home()).unwrap().is_empty());
    assert!(
      scan_forms(
        &format!(r"find / \( -path '/proc' -o -path '{}/.charliebot/sessions' \) -prune -o -name X -print", home()),
        &home()
      )
      .unwrap()
      .is_empty()
    );
    // Quoted prose holding a space is not shell, so a root written inside it is not a search root.
    assert!(
      scan_forms(&format!(r"rg -l 'find / and replace' {}/.charliebot/sessions -g metadata.json", home()), &home())
        .unwrap()
        .is_empty()
    );
  }

  #[test]
  fn rg_depth_flags_bound_a_rooted_search() {
    for command in ["rg --max-depth 3 -l X ~", "rg --max-depth=3 -l X ~", "rg -d 2 -l X /"] {
      assert!(scan_forms(command, &home()).unwrap().is_empty(), "{command}");
    }
    // Without a depth flag the same searches refuse, and grep's `-d` takes an action word, not a depth.
    for command in ["rg -l X ~", "rg -l X /", "grep -d skip -r X /"] {
      assert!(!scan_forms(command, &home()).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn finds_that_walk_the_store_and_glob_reads_of_run_logs_refuse() {
    let denied = [
      (format!("find {}/.charliebot -newermt @1 -name X", home()), ScanForm::FindWalk),
      (format!("find {}/.charliebot/sessions -name metadata.json -exec grep -l X {{}} +", home()), ScanForm::FindRead),
      (format!("cat {}/.charliebot/sessions/*/data/runs/*/events.jsonl", home()), ScanForm::RunLogGlob),
      // The apostrophe inside double quotes does not open a single-quoted span that hides the find after it.
      (format!("echo \"session's note\"; find {}/.charliebot -newermt @1 -name X", home()), ScanForm::FindWalk),
      // A `|` inside double quotes is not a pipe, so the recursive grep still reaches the store.
      (format!(r#"grep -rn "a\|b" {}/.charliebot/sessions"#, home()), ScanForm::RunLogSearch),
    ];
    for (command, expected) in denied {
      assert_eq!(scan_forms(&command, &home()).unwrap().first(), Some(&expected), "{command}");
    }
  }

  #[test]
  fn bounded_walks_and_metadata_globs_pass() {
    let commands = [
      format!("find {}/.charliebot/sessions -maxdepth 2 -name X", home()),
      format!("cat {}/.charliebot/sessions/*/data/runs/*/metadata.json", home()),
      format!("cat {}/.charliebot/sessions/*/data/runs/abc123/events.jsonl", home()),
    ];
    for command in commands {
      assert!(scan_forms(&command, &home()).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn relative_paths_resolve_against_the_hook_cwd_and_the_latest_cd() {
    // `cd ..` moves into the store, so the bare `.` is the store and the find walks it.
    let cwd = format!("{}/.charliebot/sessions/abc", home());
    assert_eq!(scan_forms("cd .. && find . -name X", &cwd).unwrap().first(), Some(&ScanForm::FindWalk));
    // Three levels up from a home-level cwd is `/home`, which the rooted search refuses.
    assert_eq!(scan_forms("find ../../../ -name X", "/home/a/b/c").unwrap().first(), Some(&ScanForm::RootedSearch));
  }

  #[test]
  fn non_bash_hook_event_prints_nothing() {
    let (result, output) = run_input(r#"{"tool_name":"Read","tool_input":{}}"#);
    result.unwrap();
    assert!(output.is_empty());
  }

  #[test]
  fn invalid_json_returns_a_stderr_cli_error_without_a_deny_decision() {
    let (result, output) = run_input("not json");
    let error = result.unwrap_err();
    assert_eq!(error.code, 1);
    assert!(error.body["error"].as_str().unwrap().starts_with("invalid hook JSON:"));
    assert!(output.is_empty());
  }

  #[test]
  fn bash_event_without_a_string_command_or_cwd_returns_a_stderr_cli_error() {
    for (input, field) in [
      (r#"{"tool_name":"Bash"}"#, "tool_input.command"),
      (r#"{"tool_name":"Bash","tool_input":{"command":61},"cwd":"/"}"#, "tool_input.command"),
      (r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#, "cwd"),
    ] {
      let (result, output) = run_input(input);
      let error = result.unwrap_err();
      assert_eq!(error.code, 1);
      assert!(error.body["error"].as_str().unwrap().contains(field));
      assert!(output.is_empty());
    }
  }

  #[test]
  fn refused_command_prints_the_exact_deny_json() {
    let (result, output) = run_input(&hook_input(r"rg -l X ~/.charliebot/sessions -g '*.jsonl'"));
    result.unwrap();
    let actual: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
      actual,
      json!({
        "hookSpecificOutput": {
          "hookEventName": "PreToolUse",
          "permissionDecision": "deny",
          "permissionDecisionReason": RUN_LOG_SEARCH_REASON
        }
      })
    );
  }

  #[test]
  fn allowed_command_prints_nothing() {
    let (result, output) =
      run_input(&hook_input("charliebot session tree --root 28f1df1f --include-archived --limit 500"));
    result.unwrap();
    assert!(output.is_empty());
  }
}
