//! The `PreToolUse` hook for unbounded scans in Bash commands, for Claude Code and codex.
//! A refused scan is one of six shell-level forms. One is one process per session directory.
//! Another is a search from `/`, `/home` or the home directory that neither limits depth nor
//! prunes the session store. A third is a recursive search of the session store or a parent of
//! it whose file-name filter reaches run logs. A fourth is a find that walks the store or a
//! parent of it with no depth limit. A fifth is that same find with a command on each file.
//! The last is one process that reads the run logs of every session through a glob.
//! An argument's quoting does not change its meaning, so a quoted store path counts as its
//! bare form. Script code inside quotes or heredoc bodies is not shell, and a relative path
//! resolves against the hook's cwd and the latest `cd`. Two bounded lookups stay allowed. One
//! is a loop whose body only tests one session's top-level `metadata.json` with shell builtins.
//! The other is a glob under one session id prefix of at least eight hex characters.

mod shell;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::OnceLock;

use charliebot_claude_code::claude_launch;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::config::SESSIONS_DIR_NAME;
use charliebot_core::event_types;
use charliebot_core::home::{CHARLIEBOT_HOME_ENV, DEFAULT_HOME_DIR_NAME};
use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_core::str_methods;
use charliebot_store::layout::DATA_DIR_NAME;
use charliebot_store::run_logs::COMPRESSED_SUFFIX;
use charliebot_store::run_records::{RAW_LOG_NAME, RUN_EVENTS_NAME, STDERR_LOG_NAME};
use regex::{Captures, Regex};
use serde_json::{Value, json};
use shell::{needs_path_resolution, parse_error, resolve_paths, shell_text, strip_quotes};

/// A recursive `grep`: `-r`, `-R` or `--recursive` among its options, alone or grouped with other flags. The
/// command-word patterns match every `grep` and this checks the flag after the match, so the search patterns stay
/// inside the fast regular-expression engine's syntax.
const RECURSIVE_GREP: &str = r"grep\b[^|;&\n]*?\s(?:--recursive|-[a-zA-Z]*[rR][a-zA-Z]*)(?:[\s]|$)";

/// The file names a run directory holds that a glob can reach as run logs, and examples of them for name matching.
///
/// The transport rotation numbers a stale raw log with a suffix, so the rotated example derives from the raw log name
/// that `charliebot_store::run_records` owns.
fn run_log_examples() -> Vec<String> {
  let plain =
    [RUN_EVENTS_NAME.to_string(), RAW_LOG_NAME.to_string(), format!("{RAW_LOG_NAME}.1"), STDERR_LOG_NAME.to_string()];
  let compressed = plain.iter().map(|name| format!("{name}{COMPRESSED_SUFFIX}")).collect::<Vec<_>>();
  compressed.into_iter().chain(plain).collect()
}

/// The commands that read a file's text, for the glob rule: a glob given to one of them reads every file it names.
const READERS: &[&str] = &[
  "grep",
  "egrep",
  "fgrep",
  "rg",
  "ag",
  "ack",
  "cat",
  "head",
  "tail",
  "jq",
  "sed",
  "awk",
  "less",
  "more",
  "wc",
  "sort",
  "uniq",
  "cut",
  "tr",
  "xargs",
  "python3",
  "python",
  "node",
  "zcat",
  "strings",
  "diff",
  "cmp",
  "nl",
  "tac",
  "od",
  "xxd",
  "sha256sum",
  "md5sum",
];

/// The wrappers a reader may follow, which the glob rule skips.
const WRAPPERS: &[&str] = &["sudo", "timeout", "nice", "ionice", "time", "env"];

/// The shell builtins a bounded metadata loop's body may run. Any other command in the body starts a process per
/// session, so the body is refused.
const LOOP_BUILTINS: &[&str] =
  &["[", "test", "echo", "printf", "true", "false", ":", "if", "then", "else", "elif", "fi"];

/// The characters that end one command in a loop body, so that each command's first word is found.
const LOOP_COMMAND_END: &[char] = &[';', '&', '|', '(', ')', '{', '}', '`', '\n'];

/// Some text in a loop's item list reads more than each session's entry: it reads file contents, reaches below a
/// session directory, or walks the store recursively. The text is a content reader, a session glob with a path under
/// it, or `ls -R`.
const LIST_READS_BELOW: &str = concat!(
  r"\b(?:grep|egrep|fgrep|rg|ag|ack|cat|head|tail|jq|python3?|node|xargs|find|du)\b",
  r#"|\*/[^\s|;&)'"`]"#,
  r"|\bls\b[^|;&\n]*\s-[a-zA-Z]*R",
);

/// The shortest literal session-id prefix that identifies one session: every session id is a UUID, whose first eight
/// hex characters are enough to name it.
const SESSION_PREFIX_MIN: usize = 8;

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
  if event.get(event_types::TOOL_NAME).and_then(Value::as_str) != Some("Bash") {
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
  let resolved =
    if needs_path_resolution(&shell) { resolve_paths(&shell, cwd, store_root_pattern()?) } else { shell.clone() };
  let mut forms = search_forms(&shell)?;
  if resolved != shell {
    for form in search_forms(&resolved)? {
      if !forms.contains(&form) {
        forms.push(form);
      }
    }
  }
  Ok(forms)
}

fn search_forms(shell: &str) -> Result<Vec<ScanForm>, String> {
  let mut forms = Vec::new();
  if ["for", "while", "xargs", "find"].iter().any(|word| shell.contains(word))
    && per_file_process_pattern()?.is_match(shell)
    && !bounded_metadata_loops(shell)?
  {
    forms.push(ScanForm::PerFileProcess);
  }
  if (shell.contains('/') || shell.contains('~') || shell.contains("$HOME"))
    && ["find", "rg", "grep", "fd"].iter().any(|word| shell.contains(word))
  {
    for captures in rooted_search_pattern()?.captures_iter(shell) {
      if non_recursive_grep(&captures)? {
        continue;
      }
      let around = groups_text(&captures);
      let prunes_store = around.contains("-prune") && around.contains(SESSIONS_DIR_NAME);
      let bounded_depth = depth_limited(&around)?;
      if !bounded_depth && !prunes_store {
        forms.push(ScanForm::RootedSearch);
        break;
      }
    }
  }
  if ["rg", "grep"].iter().any(|word| shell.contains(word))
    && (shell.contains(DEFAULT_HOME_DIR_NAME) || shell.contains(CHARLIEBOT_HOME_ENV))
  {
    for captures in run_log_search_pattern()?.captures_iter(shell) {
      if non_recursive_grep(&captures)? {
        continue;
      }
      let globs = glob_names(&groups_text(&captures));
      if globs.is_empty() || globs.iter().any(|glob| is_run_log_glob(glob)) {
        forms.push(ScanForm::RunLogSearch);
        break;
      }
    }
  }
  let mut walks_store = false;
  let mut runs_per_file = false;
  if shell.contains("find") {
    for segment in find_segments(shell) {
      if walks_store_root(&segment)? {
        walks_store = true;
        runs_per_file |= per_file_command_pattern().is_match(&segment.region);
      }
    }
  }
  if runs_per_file {
    forms.push(ScanForm::FindRead);
  }
  if walks_store {
    forms.push(ScanForm::FindWalk);
  }
  if shell.contains(['*', '?', '['])
    && (shell.contains(DEFAULT_HOME_DIR_NAME) || shell.contains(CHARLIEBOT_HOME_ENV))
    && reads_run_logs_by_glob(shell)?
  {
    forms.push(ScanForm::RunLogGlob);
  }
  Ok(forms)
}

fn groups_text(captures: &Captures<'_>) -> String {
  [1, 2].into_iter().filter_map(|index| captures.get(index)).map(|group| group.as_str()).collect()
}

fn glob_names(text: &str) -> Vec<String> {
  glob_pattern()
    .captures_iter(text)
    .map(|captures| captures.get(1).expect("glob pattern has a name group").as_str().to_string())
    .collect()
}

/// Whether a file-name filter reaches run logs: one of the run-log names, or a pattern that matches one of their
/// examples.
fn is_run_log_glob(name: &str) -> bool {
  run_log_globs().iter().any(|glob| glob == name)
    || run_log_examples().iter().any(|example| wildcard_matches(name, example))
}

/// The file-name filters that reach run logs whole, including the raw log's name with any suffix and the compressed
/// forms the compaction leaves in place of a finished run's logs.
fn run_log_globs() -> Vec<String> {
  vec![
    "*".to_string(),
    "**".to_string(),
    "*.jsonl".to_string(),
    "*.ndjson".to_string(),
    "*.json".to_string(),
    RUN_EVENTS_NAME.to_string(),
    format!("{RUN_EVENTS_NAME}{COMPRESSED_SUFFIX}"),
    RAW_LOG_NAME.to_string(),
    format!("{RAW_LOG_NAME}*"),
    "*.log".to_string(),
    format!("*{COMPRESSED_SUFFIX}"),
  ]
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

fn find_segments(shell: &str) -> Vec<FindSegment> {
  let text = without_prose(shell);
  let mut segments = Vec::new();
  for word in find_word_pattern().find_iter(&text) {
    let rest = &text[word.end()..];
    let region = find_region_pattern().find(rest).map_or("", |region| region.as_str());
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
  segments
}

/// A find that walks the session store or a parent of it: a store root, with no `-maxdepth` and no prune of `sessions`.
fn walks_store_root(segment: &FindSegment) -> Result<bool, String> {
  let mut names_store = false;
  for root in &segment.roots {
    if store_root_pattern()?.is_match(root.trim_end_matches('/')) {
      names_store = true;
      break;
    }
  }
  let prunes_store = segment.args.contains("-prune") && segment.args.contains(SESSIONS_DIR_NAME);
  Ok(names_store && !segment.args.contains("-maxdepth") && !prunes_store)
}

/// Whether a reader command's glob names run logs: one process that reads the run logs of every session.
fn reads_run_logs_by_glob(shell: &str) -> Result<bool, String> {
  if !shell.contains('"') {
    let text = shell;
    return read_run_log_glob_segments(text);
  }
  let text = without_prose(shell);
  read_run_log_glob_segments(&text)
}

fn read_run_log_glob_segments(text: &str) -> Result<bool, String> {
  for segment in command_segment_pattern().find_iter(text) {
    if !is_reader_segment(segment.as_str()) {
      continue;
    }
    for token in segment
      .as_str()
      .split(|character: char| character.is_whitespace() || ";&|()<>`".contains(character))
      .filter(|token| !token.is_empty())
    {
      if reaches_run_logs(&strip_quotes(token))? {
        return Ok(true);
      }
    }
  }
  Ok(false)
}

/// The shell text with each double-quoted run of prose replaced by a space: a quoted string holding a space or a
/// separator is not one shell argument. A string holding a command substitution or backtick stays, because that is
/// shell.
fn without_prose(shell: &str) -> String {
  if !shell.contains('"') {
    return shell.to_string();
  }
  prose_pattern()
    .replace_all(shell, |captures: &Captures<'_>| {
      let whole = captures.get(0).expect("group 0 is the whole match").as_str();
      let inner = captures.get(1).map_or("", |inner| inner.as_str());
      let prose = inner.contains(|character: char| character.is_whitespace() || ";&|()<>".contains(character));
      if prose && !inner.contains("$(") && !inner.contains('`') { " ".to_string() } else { whole.to_string() }
    })
    .into_owned()
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
    return !name.is_empty() && name.chars().all(str_methods::is_word_char);
  }
  let digits = word.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(word);
  !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit())
}

/// Whether one glob token spans every session and reaches run logs. The token must start at the session store, and
/// the session directory just under it must be a wildcard. A literal session id, or a prefix long enough to name one
/// session, is one session's lookup. Under `data/runs/` the run id must be a wildcard too. The token reaches run
/// logs when it names the run directory itself or a run-log file, but not `metadata.json`. Elsewhere under the
/// session wildcard, a name that is a run-log name counts.
fn reaches_run_logs(token: &str) -> Result<bool, String> {
  if token.starts_with('!') {
    return Ok(false);
  }
  let Some(under_store) = store_remainder(token)? else {
    return Ok(false);
  };
  let parts: Vec<&str> = under_store.split('/').collect();
  let Some((&session, under_session)) = parts.split_first() else {
    return Ok(false);
  };
  if !is_wild(session) || names_one_session(session) {
    return Ok(false);
  }
  if under_session.starts_with(&["data", "runs"]) {
    let rest = &under_session[2..];
    let run_directory_glob = rest.first().is_some_and(|first| is_wild(first));
    let names_run_log = rest.len() == 1 || rest.last().is_some_and(|last| is_run_log_glob(last));
    return Ok(run_directory_glob && rest.last() != Some(&METADATA_FILE_NAME) && names_run_log);
  }
  Ok(parts.last().is_some_and(|last| is_run_log_glob(last)))
}

/// The part of a token under the session store: what follows `sessions/`, or `None` when the token is not in the
/// store at all.
fn store_remainder(token: &str) -> Result<Option<&str>, String> {
  let Some(prefix) = store_prefix_pattern()?.find(token) else {
    return Ok(None);
  };
  Ok(Some(token[prefix.end()..].trim_start_matches('/')))
}

fn is_wild(component: &str) -> bool {
  component.contains(['*', '?', '['])
}

/// Whether a session component's literal prefix names one session: at least [`SESSION_PREFIX_MIN`] characters, all hex
/// digits or hyphens, which is how an agent spells a session id prefix. A prefix that short or shaped differently
/// could match many sessions, so it stays a wildcard.
fn names_one_session(component: &str) -> bool {
  let prefix: String = component.chars().take_while(|character| !matches!(character, '*' | '?' | '[')).collect();
  prefix.len() >= SESSION_PREFIX_MIN
    && prefix.chars().all(|character| character.is_ascii_hexdigit() || character == '-')
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
    charliebot_dir = DEFAULT_HOME_DIR_NAME,
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
    charliebot_dir = DEFAULT_HOME_DIR_NAME,
    charliebot_home = regex::escape(CHARLIEBOT_HOME_ENV),
    sessions = regex::escape(SESSIONS_DIR_NAME),
  ))
}

/// An enumeration over every session directory: a store glob, `*/metadata.json` or `*/data/...` after a `cd` into the
/// store, or `$(ls|find ... <store>)`.
fn enumeration_token(store: &str) -> String {
  format!(
    concat!(r"(?:", r"{store}/\*", r"|(?:^|[^\w/])\*/(?:{metadata}|{data}/)", r"|\$\((?:ls|find)\s[^)]*{store}", r")"),
    store = store,
    metadata = regex::escape(METADATA_FILE_NAME),
    data = regex::escape(DATA_DIR_NAME),
  )
}

/// Whether every per-session form the command holds is a bounded metadata loop. A loop of the form
/// `ls */ | ... | while read id; do [ -f "$id/metadata.json" ] ...; done` lists the store's top level in one process.
/// It only tests one top-level file per session with shell builtins, so its work does not grow with the store's file
/// contents. The same body on a `for ... in */` list stays refused: that form is also the one-process-per-session
/// shape. A list that reads contents or reaches below a session directory, and a body that runs any command outside
/// the builtins, keep the refusal. Any other per-session form left after the bounded loops are masked keeps it too.
fn bounded_metadata_loops(shell: &str) -> Result<bool, String> {
  let mut spans = Vec::new();
  for captures in metadata_loop_pattern()?.captures_iter(shell) {
    let whole = captures.get(0).expect("the metadata loop pattern has a whole match");
    let body = captures.name("body").map_or("", |body| body.as_str());
    let list_end = captures.name("list").map_or(whole.end(), |list| list.end());
    let segment_start = shell[..whole.start()].rfind([';', '&', '\n']).map_or(0, |at| at + 1);
    let list = &shell[segment_start..list_end];
    if !body.contains(METADATA_FILE_NAME) || !loop_body_is_builtin(body) || list_reads_below_pattern()?.is_match(list) {
      continue;
    }
    spans.push((whole.start(), whole.end()));
  }
  if spans.is_empty() {
    return Ok(false);
  }
  let mut masked = String::with_capacity(shell.len());
  let mut last = 0;
  for (start, end) in spans {
    masked.push_str(&shell[last..start]);
    masked.push_str(&" ".repeat(shell[start..end].chars().count()));
    last = end;
  }
  masked.push_str(&shell[last..]);
  Ok(!per_file_process_pattern()?.is_match(&masked))
}

/// (a) One process per session directory at the shell level. The forms are a `for` loop over an enumeration, an
/// enumeration piped to `while read`, `xargs -n1` after a store path, and `find <store> -exec`.
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

/// A `| while read` loop over an enumeration, with the item list before the pipe and the body after `do`, up to a
/// `done` that ends a command.
fn metadata_loop_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      Regex::new(&format!(
        concat!(r"(?s)(?P<list>{enumeration}[^;\n]*)\|\s*while\s+read\b[^;]*;\s*do\b", r"(?P<body>.*?)[;&\n]\s*done\b"),
        enumeration = enumeration_token(&store_token()?)
      ))
      .map_err(parse_error)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Whether a loop body runs only shell builtins and reads no file inside the shell. A `<` redirection,
/// `read < file` or `$(< file)` would read a session's contents with no process to show it.
fn loop_body_is_builtin(body: &str) -> bool {
  !body.contains('<')
    && body.split(LOOP_COMMAND_END).all(|command| {
      match command.split_whitespace().map(|word| word.trim_start_matches('!')).find(|word| !word.is_empty()) {
        Some(word) => LOOP_BUILTINS.contains(&word),
        None => true,
      }
    })
}

fn list_reads_below_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(LIST_READS_BELOW).map_err(parse_error)).as_ref().map_err(Clone::clone)
}

/// (b) A search from `/`, `/home` or the home directory itself, with its arguments on both sides.
fn rooted_search_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      Regex::new(&format!(
        concat!(r"\b(?:find|rg|grep|fd)\b([^|;&\n]*?)", r"\s(?:/home/?|/|~|~/|\$HOME|{home}/?)(?:[\s)]|$)([^|;&\n]*)"),
        home = home_token()?
      ))
      .map_err(parse_error)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Whether the match's command word is a `grep` that holds no recursive flag: the arm matches every grep, and a
/// non-recursive grep is no refused form. `whole` is the match, which starts at the command word.
fn non_recursive_grep(captures: &Captures<'_>) -> Result<bool, String> {
  let whole = captures.get(0).expect("group 0 is the whole match").as_str();
  if !whole.starts_with("grep") {
    return Ok(false);
  }
  Ok(!recursive_flag_pattern()?.is_match(whole))
}

/// A recursive `grep` inside one command segment: `grep` with `-r`, `-R` or `--recursive` later in the segment.
fn recursive_flag_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(RECURSIVE_GREP).map_err(parse_error)).as_ref().map_err(Clone::clone)
}

/// Whether the search bounds its depth: find's `-maxdepth`, or rg's and fd's `--max-depth` (spaced or with `=`) and
/// `-d N`. grep's `-d` takes an action word, not a depth, so the short flag needs a number.
fn depth_limited(search_text: &str) -> Result<bool, String> {
  if search_text.contains("-maxdepth") {
    return Ok(true);
  }
  Ok(depth_flag_pattern()?.is_match(search_text))
}

fn depth_flag_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| Regex::new(concat!(r"(?:^|[^\w-])-d\s*\d", r"|--max-depth(?:\s+|=)\d")).map_err(parse_error))
    .as_ref()
    .map_err(Clone::clone)
}

/// (c) A recursive search of the store or a parent of it, with its arguments on both sides.
fn run_log_search_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| {
      Regex::new(&format!(
        concat!(r"\b(?:rg|grep)\b([^|;&\n]*?)", r#"\s\"?(?:{store_or_parent})/?\"?(?:[\s)]|$)([^|;&\n]*)"#),
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

/// The session store at the start of a token, leaving what follows `sessions/`.
fn store_prefix_pattern() -> Result<&'static Regex, String> {
  static PATTERN: OnceLock<Result<Regex, String>> = OnceLock::new();
  PATTERN
    .get_or_init(|| Regex::new(&format!("^(?:{})(?:/|$)", store_token()?)).map_err(parse_error))
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
  PATTERN
    .get_or_init(|| Regex::new(r#"(?:-g|--glob|--include)[ =]['"]?([^'" !][^'" ]*)"#).expect("glob pattern is valid"))
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
  fn per_file_process_replay_case_matches_its_scan_form() {
    // The replay's first real command: a metadata loop after a `cd` into the store.
    let command = r#"cd ~/.charliebot/sessions && for f in */metadata.json; do python3 -I -c "import json; print(json.load(open('$f')))"; done"#;
    let forms = scan_forms(command, &home()).unwrap();
    assert_eq!(forms.first(), Some(&ScanForm::PerFileProcess), "{command}");
  }

  #[test]
  fn rooted_search_replay_case_matches_its_scan_form() {
    let command = "find / -path /proc -prune -o -type d -name dataviz -print 2>/dev/null | head -3";
    let forms = scan_forms(command, &home()).unwrap();
    assert_eq!(forms.first(), Some(&ScanForm::RootedSearch), "{command}");
  }

  #[test]
  fn store_search_cases_match_the_replay_table() {
    let command =
      r#"rg -o 'ur-[A-Za-z0-9._-]+' "$CHARLIEBOT_HOME/sessions" ~/.charlie-code/sessions 2>/dev/null | sort -u"#;
    let forms = scan_forms(command, &home()).unwrap();
    assert_eq!(forms.first(), Some(&ScanForm::RunLogSearch), "{command}");
  }

  #[test]
  fn heredoc_loop_cases_match_the_replay_table() {
    // A run-record loop whose Python body is a heredoc, not shell.
    let command = "cd ~/.charliebot/sessions && for m in */data/runs/*/metadata.json; do python3 - \"$m\" <<'EOF2'\nimport json\nd = json.load(open(\"$m\"))\nprint(d)\nEOF2\ndone";
    let forms = scan_forms(command, &home()).unwrap();
    assert_eq!(forms.first(), Some(&ScanForm::PerFileProcess), "{command}");
  }

  #[test]
  fn per_session_process_cases_match_the_replay_table() {
    let denied = [ScanForm::PerFileProcess, ScanForm::PerFileProcess];
    let commands: [&str; 2] = [
      r#"for d in $(ls ~/.charliebot/sessions); do python3 -I -c 'print(1)' $d/metadata.json; done"#,
      "find ~/.charliebot/sessions -name metadata.json | xargs -n1 python3 -I x.py",
    ];
    for (command, expected) in commands.iter().zip(denied) {
      let forms = scan_forms(command, &home()).unwrap();
      assert_eq!(forms.first(), Some(&expected), "{command}");
    }
  }

  #[test]
  fn the_rest_of_the_replay_table_matches_its_scan_forms() {
    let denied = [ScanForm::RunLogSearch, ScanForm::RunLogSearch];
    let commands: [&str; 2] =
      [r"rg -l X ~/.charliebot/sessions -g '*.jsonl'", r"rg -l X ~/.charliebot/sessions -g 'agent.raw.ndjson'"];
    for (command, expected) in commands.iter().zip(denied) {
      let forms = scan_forms(command, &home()).unwrap();
      assert_eq!(forms.first(), Some(&expected), "{command}");
    }
  }

  #[test]
  fn grep_loop_and_home_search_cases_match_the_replay_table() {
    let denied = [ScanForm::PerFileProcess, ScanForm::RunLogSearch];
    let commands: [&str; 2] = [
      r"grep -l 520ad103 ~/.charliebot/sessions/*/metadata.json | while read f; do stat $f; done",
      // The absolute home form, spelled through the test home.
      &format!("grep -rl '39828874' {}/.charliebot/ 2>/dev/null", home()),
    ];
    for (command, expected) in commands.iter().zip(denied) {
      let forms = scan_forms(command, &home()).unwrap();
      assert_eq!(forms.first(), Some(&expected), "{command}");
    }
  }

  #[test]
  fn per_session_glob_loop_is_refused() {
    let command = r#"cd ~/.charliebot/sessions && for f in */metadata.json; do python3 -I -c "import json,sys; print(json.load(open('$f')).get('title'))"; done"#;
    let forms = scan_forms(command, &home()).unwrap();
    assert_eq!(forms.first(), Some(&ScanForm::PerFileProcess), "{command}");
  }

  #[test]
  fn rotated_raw_log_search_is_refused() {
    let command = r"rg -l X ~/.charliebot/sessions -g 'agent.raw.ndjson.1'";
    let forms = scan_forms(command, &home()).unwrap();
    assert_eq!(forms.first(), Some(&ScanForm::RunLogSearch), "{command}");
  }

  #[test]
  fn bounded_lookup_cases_hold_no_scan_form() {
    let commands: [&str; 5] = [
      "charliebot session tree --root 28f1df1f --include-archived --limit 500",
      r"rg -l -F X ~/.charliebot/sessions -g 'chat_events*.jsonl'",
      r"rg -l -F X ~/.charliebot/sessions -g '*.md'",
      "find / -maxdepth 4 -name X",
      &format!(r"find / \( -path /proc -o -path {}/.charliebot/sessions \) -prune -o -name X -print", home()),
    ];
    for command in commands {
      assert!(scan_forms(command, &home()).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn bounded_glob_and_loop_lookups_hold_no_scan_form() {
    let commands: [&str; 5] = [
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
  fn recursive_grep_over_the_store_is_refused() {
    let denied = [
      "grep -n -r X ~/.charliebot/sessions",
      "grep -R X ~/.charliebot/sessions",
      "grep --recursive X ~/.charliebot/sessions",
    ];
    for command in denied {
      assert!(!scan_forms(command, &home()).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn unpruned_home_and_multi_root_searches_are_refused() {
    let denied =
      [r#"find ~ -path "*/sessions/*" -name metadata.json"#, "rg -l X / ~/.charliebot/sessions -g metadata.json"];
    for command in denied {
      assert!(!scan_forms(command, &home()).unwrap().is_empty(), "{command}");
    }
    assert!(scan_forms("grep -n -r X ~/workspace/charlie-bot/rust", &home()).unwrap().is_empty());
  }

  #[test]
  fn a_single_quoted_store_path_keeps_its_run_log_glob() {
    let home = home();
    let command = format!(r"rg -l X '{home}/.charliebot/sessions' -g '*.jsonl'");
    assert_eq!(scan_forms(&command, &home).unwrap(), vec![ScanForm::RunLogSearch]);
  }

  #[test]
  fn a_single_quoted_prune_path_keeps_its_prune() {
    let command = r"find ~ -path '*/.charliebot/sessions' -prune -o -name X -print";
    assert!(scan_forms(command, &home()).unwrap().is_empty(), "{command}");
  }

  #[test]
  fn an_absolute_single_quoted_prune_path_keeps_its_prune() {
    let home = home();
    let command =
      format!(r"find / \( -path '/proc' -o -path '{home}/.charliebot/sessions' \) -prune -o -name X -print");
    assert!(scan_forms(&command, &home).unwrap().is_empty(), "{command}");
  }

  #[test]
  fn quoted_prose_does_not_hide_a_store_search() {
    let home = home();
    let command = format!(r"rg -l 'find / and replace' {home}/.charliebot/sessions -g metadata.json");
    assert!(scan_forms(&command, &home).unwrap().is_empty(), "{command}");
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
  fn a_find_walk_of_the_store_is_refused() {
    let home = home();
    let command = format!("find {home}/.charliebot -newermt @1 -name X");
    assert_eq!(scan_forms(&command, &home).unwrap(), vec![ScanForm::FindWalk]);
  }

  #[test]
  fn a_find_read_of_the_store_is_refused() {
    let home = home();
    let command = format!("find {home}/.charliebot/sessions -name metadata.json -exec grep -l X {{}} +");
    assert_eq!(scan_forms(&command, &home).unwrap(), vec![ScanForm::FindRead, ScanForm::FindWalk]);
  }

  #[test]
  fn a_run_log_glob_read_is_refused() {
    let home = home();
    let command = format!("cat {home}/.charliebot/sessions/*/data/runs/*/events.jsonl");
    assert_eq!(scan_forms(&command, &home).unwrap(), vec![ScanForm::RunLogGlob]);
  }

  #[test]
  fn an_apostrophe_in_double_quoted_prose_does_not_hide_a_find_walk() {
    let home = home();
    let command = format!("echo \"session's note\"; find {home}/.charliebot -newermt @1 -name X");
    assert_eq!(scan_forms(&command, &home).unwrap(), vec![ScanForm::FindWalk]);
  }

  #[test]
  fn a_pipe_in_double_quoted_prose_does_not_hide_a_run_log_search() {
    let home = home();
    let command = format!(r#"grep -rn "a\|b" {home}/.charliebot/sessions"#);
    assert_eq!(scan_forms(&command, &home).unwrap(), vec![ScanForm::RunLogSearch]);
  }

  /// The compressed names the compaction leaves in place of a finished run's logs are refused exactly as the plain
  /// names are.
  #[test]
  fn run_log_globs_reach_the_compressed_names() {
    let home = home();
    let denied = [
      (format!("cat {home}/.charliebot/sessions/*/data/runs/*/events.jsonl.zst"), ScanForm::RunLogGlob),
      (format!("cat {home}/.charliebot/sessions/*/data/runs/*/agent.raw.ndjson.1.zst"), ScanForm::RunLogGlob),
      (format!("rg -l X {home}/.charliebot/sessions -g '*.zst'"), ScanForm::RunLogSearch),
      (format!("rg -l X {home}/.charliebot/sessions -g 'agent.raw.ndjson.zst'"), ScanForm::RunLogSearch),
    ];
    for (command, expected) in denied {
      assert_eq!(scan_forms(&command, &home).unwrap().first(), Some(&expected), "{command}");
    }
  }

  #[test]
  fn bounded_walks_and_metadata_globs_pass() {
    let commands = [
      format!("find {}/.charliebot/sessions -maxdepth 2 -name X", home()),
      format!("cat {}/.charliebot/sessions/*/data/runs/*/metadata.json", home()),
      format!("cat {}/.charliebot/sessions/*/data/runs/abc123/events.jsonl", home()),
      // A glob outside the store, and one that names one session, does not span every session.
      format!("cat {}/.claude/projects/*/*.jsonl", home()),
      format!("cat {}/.charliebot/sessions/abc/data/runs/*/events.jsonl", home()),
      // A double-quoted run of prose holding a path is not shell, so its `find` is not a walk.
      format!(r#"echo "find {}/.charliebot/sessions and replace""#, home()),
    ];
    for command in commands {
      assert!(scan_forms(&command, &home()).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn a_bounded_metadata_loop_passes() {
    let home = home();
    // A top-level listing whose body only tests one session's metadata.json with shell builtins.
    let command = format!(
      "cd {home}/.charliebot/sessions && ls -d */ | while read -r id; do [ -f \"$id/metadata.json\" ] && echo \"$id\"; done"
    );
    assert!(scan_forms(&command, &home).unwrap().is_empty(), "{command}");
  }

  #[test]
  fn session_id_prefix_globs_pass() {
    let home = home();
    let passed = [
      // A glob under one session id prefix names one session, not every session.
      format!("grep -l X {home}/.charliebot/sessions/dfb20fec*/specs/*"),
      format!("grep -l X {home}/.charliebot/sessions/d7645e8f-*/data/runs/*/events.jsonl"),
    ];
    for command in passed {
      assert!(scan_forms(&command, &home).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn per_session_processes_are_refused() {
    let home = home();
    let denied = [
      // The same builtin body on a `for ... in */` list is the one-process-per-session shape.
      format!("cd {home}/.charliebot/sessions && for id in */; do [ -f \"$id/metadata.json\" ] && echo \"$id\"; done"),
      // A body that starts a process for every matched session.
      format!(
        "cd {home}/.charliebot/sessions && ls -d */ | while read -r id; do jq -r .name \"$id/metadata.json\"; done"
      ),
      // A `done` inside an echo does not end the body, so the jq after it still runs once per session.
      format!(
        "cd {home}/.charliebot/sessions && ls -d */ | while read -r id; do [ -f \"$id/metadata.json\" ] && echo done; jq -r .name \"$id/metadata.json\"; done"
      ),
    ];
    for command in denied {
      assert!(!scan_forms(&command, &home).unwrap().is_empty(), "{command}");
    }
  }

  #[test]
  fn a_process_per_session_is_refused() {
    let home = home();
    let command = format!(
      "cd {home}/.charliebot/sessions && ls -d */ | while read -r id; do [ -f \"$id/metadata.json\" ] && perl -ne 1 \"$id/metadata.json\"; done"
    );
    assert!(!scan_forms(&command, &home).unwrap().is_empty(), "{command}");
  }

  #[test]
  fn a_builtin_redirect_per_session_is_refused() {
    let home = home();
    let command = format!(
      "cd {home}/.charliebot/sessions && ls -d */ | while read -r id; do read -r line < \"$id/metadata.json\"; echo \"$line\"; done"
    );
    assert!(!scan_forms(&command, &home).unwrap().is_empty(), "{command}");
  }

  #[test]
  fn a_producer_reading_session_metadata_is_refused() {
    let home = home();
    let command = format!(
      "cat {home}/.charliebot/sessions/*/metadata.json | while read -r id; do [ -f \"$id/metadata.json\" ] && echo \"$id\"; done"
    );
    assert!(!scan_forms(&command, &home).unwrap().is_empty(), "{command}");
  }

  #[test]
  fn a_recursive_session_list_is_refused() {
    let home = home();
    // A recursive listing walks below every session directory.
    let command = format!(
      "cd {home}/.charliebot/sessions && ls -R */ | while read -r id; do [ -f \"$id/metadata.json\" ] && echo \"$id\"; done"
    );
    assert!(!scan_forms(&command, &home).unwrap().is_empty(), "{command}");
  }

  #[test]
  fn multi_session_prefix_globs_are_refused() {
    let home = home();
    let denied = [
      // A short prefix, or one that is not a session id, can match many sessions.
      format!("grep -l X {home}/.charliebot/sessions/1680*/specs/*"),
      format!("grep -l X {home}/.charliebot/sessions/session-*/specs/*"),
    ];
    for command in denied {
      assert!(!scan_forms(&command, &home).unwrap().is_empty(), "{command}");
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
