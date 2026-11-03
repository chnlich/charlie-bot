//! Run cause lines: cleaning, the ordered cause patterns, normalization, the no-cause shapes and the signature keys.
//!
//! A run's record names its error material in four places, tried in this order. The first is the error text of its
//! newest `result` event when that event reports an error. Then come the `error` events of its `events.jsonl` (newest
//! first), the end of its `agent.stderr.log`, and last the `terminal_reason` of its `result` event. Extraction picks
//! the first cause the ordered `PATTERNS` list in this file finds across those texts. A record that fits one of the
//! closed no-cause shapes has no cause to judge.

use std::sync::LazyLock;

use regex::Regex;

/// The longest normalized cause line a signature carries, in characters.
pub const MAX_CAUSE_CHARS: usize = 240;

/// The longest example error text a row carries, in characters.
pub const MAX_ANCHOR_CHARS: usize = 200;

static ANSI: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").expect("the ANSI escape expression is valid"));

/// Lines that carry no cause: the LiteLLM banner, a log-retention note and an entry-path note.
static NOISE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
  [
    r"^Provider List: https://docs\.litellm\.ai",
    r"^Give Feedback / Get Help:",
    r"^LiteLLM\.Info: If you need to debug this error",
    r"(?i)^[\w.:/-]*docs/providers$",
    r"^Command logs retained at:",
    r"^loading path=",
  ]
  .into_iter()
  .map(|pattern| Regex::new(pattern).expect("every noise expression is valid"))
  .collect()
});

/// A LiteLLM banner line, in its three-line form plus a provider-list URL whose line was cut short. A record made only
/// of these has no cause to read.
static BANNER: LazyLock<Vec<Regex>> = LazyLock::new(|| {
  [
    r"^Provider List: https://docs\.litellm\.ai",
    r"^Give Feedback / Get Help:",
    r"^LiteLLM\.Info: If you need to debug this error",
    r"(?i)^[\w.:/-]*docs/providers$",
    r"litellm\._turn_on_debug",
  ]
  .into_iter()
  .map(|pattern| Regex::new(pattern).expect("every banner expression is valid"))
  .collect()
});

static UUID: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").expect("the uuid expression is valid")
});
static SESSION_PATH: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(&format!(r"{}/<uuid>", charliebot_core::config::SESSIONS_DIR_NAME))
    .expect("the session-path expression is valid")
});
static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,40}\b").expect("the hex expression is valid"));
/// A session id field value, and the OpenCode `ses_` id form: per-session identifiers belong to no signature.
static SESSION_FIELD: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
  [
    (r"(?i)(^|\s)session_id=[^\s]+", "${1}session_id=<session>"),
    (r"(?i)(^|\s)session=[^\s]+", "${1}session=<session>"),
    (r"\bses_[A-Za-z0-9]+\b", "<session>"),
  ]
  .into_iter()
  .map(|(pattern, replacement)| (Regex::new(pattern).expect("every session-id expression is valid"), replacement))
  .collect()
});
static PATH: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r#"/home/[^\s:'"]+|/tmp/[^\s:'"]+|/Users/[^\s:'"]+"#).expect("the path expression is valid")
});
static TIMESTAMP: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"\b\d{4}-\d{2}-\d{2}T[\d:.]+Z?\b").expect("the timestamp expression is valid"));
static NUMBER_WITH_UNIT: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"\b\d+(\.\d+)?\s?(ms|s|MB|GB|KiB|MiB|B)\b").expect("the number-with-unit expression is valid")
});
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d+\b").expect("the number expression is valid"));
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("the whitespace expression is valid"));

static REDACTIONS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
  [
    (r"(?i)(api_key[=:]\s*)[A-Za-z0-9_\-]{8,}", "$1<redacted>"),
    (r"(?i)(bearer\s+)[A-Za-z0-9._\-]{8,}", "$1<redacted>"),
    (r"\bsk-[A-Za-z0-9_\-]{8,}", "<redacted>"),
    (r"(?i)(token[=:]\s*)\S{8,}", "$1<redacted>"),
  ]
  .into_iter()
  .map(|(pattern, replacement)| (Regex::new(pattern).expect("every redaction expression is valid"), replacement))
  .collect()
});
static SECRET_HEX: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"\b[0-9a-f]{32,}\b").expect("the secret-hex expression is valid"));

static HTTP_STATUS: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"\b[45]\d\d\b").expect("the http-status expression is valid"));

/// A log line's level. Only an ERROR line names a cause when the text carries levels at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogLevel {
  Error,
  Other,
}

static LEVEL_FIELD: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"(?i)\blevel=(trace|debug|info|warn|warning|error)\b").expect("the log-level field expression is valid")
});
static LEVEL_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"(^|\s)(TRACE|DEBUG|INFO|WARN|WARNING|ERROR)(\s|$)").expect("the log-level token expression is valid")
});

/// The level of one log line, or `None` when the line carries none.
fn line_level(line: &str) -> Option<LogLevel> {
  if let Some(captures) = LEVEL_FIELD.captures(line) {
    let level = captures.get(1).expect("the level is captured").as_str();
    return Some(if level.eq_ignore_ascii_case("error") { LogLevel::Error } else { LogLevel::Other });
  }
  let captures = LEVEL_TOKEN.captures(line)?;
  let level = captures.get(2).expect("the level is captured").as_str();
  Some(if level == "ERROR" { LogLevel::Error } else { LogLevel::Other })
}

/// The cause material of one error text: the text itself when no line carries a log level, or its ERROR-level lines.
/// A log dump with no ERROR line has no cause material, so extraction continues to an older error event.
fn cause_material(text: &str) -> Option<String> {
  let mut leveled = false;
  let mut errors = Vec::new();
  for line in text.lines() {
    match line_level(line) {
      Some(LogLevel::Error) => {
        leveled = true;
        errors.push(line);
      }
      Some(LogLevel::Other) => leveled = true,
      None => {}
    }
  }
  if !leveled {
    return Some(text.to_string());
  }
  if errors.is_empty() { None } else { Some(errors.join("\n")) }
}

/// What part of the cause material a pattern's hit names.
#[derive(Clone, Copy)]
enum MatchShape {
  /// The matched fragment.
  Fragment,
  /// The whole cleaned text, for the Python inventory classes whose matched fragment is too narrow to identify the
  /// failure.
  WholeText,
  /// The line holding the hit, for a class whose match is a single word.
  WholeLine,
}

/// One ordered cause pattern: its inventory class name, its expression, and the part of the cause material a hit
/// names.
struct CausePattern {
  /// The Python inventory's class name. The signature keeps the normalized cause line, not this name.
  #[allow(dead_code)]
  name: &'static str,
  expression: Regex,
  shape: MatchShape,
}

impl CausePattern {
  fn new(name: &'static str, pattern: &str, shape: MatchShape) -> CausePattern {
    CausePattern { name, expression: Regex::new(pattern).expect("every cause expression is valid"), shape }
  }
}

static PATTERNS: LazyLock<Vec<CausePattern>> = LazyLock::new(|| {
  vec![
    CausePattern::new("scope-exists", r"the previous round still runs: unit \S+sc?ope exists", MatchShape::Fragment),
    CausePattern::new("no-cron-task-file", r"no cron task file under .* names session_id", MatchShape::Fragment),
    CausePattern::new("missing-python-module", r"No module named '([^']+)'", MatchShape::Fragment),
    CausePattern::new("skill-yaml", r"failed to load skill \S+: missing YAML frontmatter", MatchShape::Fragment),
    CausePattern::new("unrecognized-model", r"\[claude-code:unrecognized_model\].*", MatchShape::Fragment),
    CausePattern::new("mem-cap", "内存上限触发", MatchShape::Fragment),
    CausePattern::new("ratelimit", r"RateLimitError:.*", MatchShape::WholeText),
    CausePattern::new(
      "overloaded",
      r"(?i)(service may be temporarily overloaded|overloaded_error|502|503)",
      MatchShape::WholeText,
    ),
    CausePattern::new(
      "auth",
      r"(?i)(Failed to authenticate|oauth_org_not_allowed|invalid x-api-key|401|Unauthorized|authentication_error)",
      MatchShape::Fragment,
    ),
    CausePattern::new(
      "timeout",
      r"(?i)(timed out|timeout after|TimeoutError|ETIMEDOUT|deadline exceeded)",
      MatchShape::Fragment,
    ),
    CausePattern::new(r"oom-kill", r"(?i)(out of memory|\bOOM\b|\bkilled\b|SIGKILL)", MatchShape::Fragment),
    CausePattern::new("traceback", r"Traceback \(most recent call last\)", MatchShape::WholeText),
    CausePattern::new("compile-error", r"(?m)^error(\[E\d+\])?:.*", MatchShape::WholeText),
    CausePattern::new("worktree", r"(?i)(worktree|already exists|not a git repository)", MatchShape::WholeText),
    CausePattern::new("assertion", r"(?i)(assertion failed|AssertionError|assert_eq)", MatchShape::WholeText),
    CausePattern::new(
      "permission",
      r"(?i)(Permission denied|EACCES|landlock|read-only file system)",
      MatchShape::WholeText,
    ),
    CausePattern::new("git", r"(?i)(fatal: |CONFLICT \(|rejected|non-fast-forward)", MatchShape::WholeText),
    CausePattern::new("api-error", r"(?i)(API Error|api_error|invalid_request_error)", MatchShape::WholeText),
    CausePattern::new("failed-word", r"(?i)(^|\W)(error|failed|failure|exception)(\W|$)", MatchShape::WholeLine),
  ]
});

/// The four shapes of a record whose cause is not in the record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoCauseShape {
  /// The error text is blank after cleaning.
  EmptyText,
  /// The error text is the LiteLLM banner alone.
  BannerOnly,
  /// The record holds a `terminal_reason` with no HTTP status.
  TerminalReason,
  /// The record holds an exit code and no error text.
  ExitCodeOnly,
}

impl NoCauseShape {
  /// The reason a row states for this shape.
  pub fn reason(self) -> &'static str {
    match self {
      NoCauseShape::EmptyText => "the record's error text is blank after cleaning",
      NoCauseShape::BannerOnly => "the record holds only the LiteLLM banner",
      NoCauseShape::TerminalReason => "the record holds only a terminal_reason with no HTTP status",
      NoCauseShape::ExitCodeOnly => "the record holds only the exit code",
    }
  }
}

/// What extraction read from one run's error material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtractedCause {
  /// A cause line to judge, with the redacted example text it came from.
  Cause { line: String, anchor: String },
  /// No cause to judge, with the shape that says why.
  NoCause { shape: NoCauseShape, anchor: String },
}

/// Drop ANSI escapes, blank lines and the noise lines that carry no cause.
pub fn clean(text: &str) -> String {
  let plain = ANSI.replace_all(text, "");
  plain
    .lines()
    .filter(|line| !line.trim().is_empty() && !NOISE.iter().any(|noise| noise.is_match(line)))
    .collect::<Vec<_>>()
    .join("\n")
}

/// Remove credentials and long identifiers from text that reaches a row or a model.
pub fn redact(text: &str) -> String {
  let mut text = text.to_string();
  for (expression, replacement) in REDACTIONS.iter() {
    text = expression.replace_all(&text, *replacement).into_owned();
  }
  SECRET_HEX.replace_all(&text, "<redacted>").into_owned()
}

/// The stable signature text of a cause line: identifiers, numbers, timestamps and per-session path segments
/// replaced, whitespace collapsed, and the first [`MAX_CAUSE_CHARS`] characters kept.
pub fn normalize(text: &str) -> String {
  let mut text = UUID.replace_all(text, "<uuid>").into_owned();
  text = SESSION_PATH.replace_all(&text, "<session>").into_owned();
  text = redact(&text);
  text = ANSI.replace_all(&text, "").into_owned();
  for (expression, replacement) in SESSION_FIELD.iter() {
    text = expression.replace_all(&text, *replacement).into_owned();
  }
  text = HEX.replace_all(&text, "<hex>").into_owned();
  text = PATH.replace_all(&text, "<path>").into_owned();
  text = TIMESTAMP.replace_all(&text, "<ts>").into_owned();
  text = NUMBER_WITH_UNIT.replace_all(&text, "<n>$2").into_owned();
  text = NUMBER.replace_all(&text, "<n>").into_owned();
  let text = WHITESPACE.replace_all(&text, " ");
  text.trim().chars().take(MAX_CAUSE_CHARS).collect()
}

/// The redacted example text a row keeps for a cause or a no-cause shape.
fn anchor(text: &str) -> String {
  redact(text).chars().take(MAX_ANCHOR_CHARS).collect()
}

/// Whether the text holds only banner lines. The banner reaches a log or an event colored, so the escapes come off
/// before the lines are read.
fn banner_only(text: &str) -> bool {
  let plain = ANSI.replace_all(text, "");
  let mut saw_banner = false;
  for line in plain.lines() {
    if line.trim().is_empty() {
      continue;
    }
    if BANNER.iter().any(|banner| banner.is_match(line)) {
      saw_banner = true;
      continue;
    }
    return false;
  }
  saw_banner
}

/// Whether an HTTP status appears in the text.
fn has_http_status(text: &str) -> bool {
  HTTP_STATUS.is_match(text)
}

/// The first cause the ordered pattern list finds in `text`. Request ids are masked first: a hex id can hold the digits
/// of a status pattern, and a per-request value names no cause.
fn match_cause(text: &str) -> Option<String> {
  let masked = UUID.replace_all(text, "<uuid>");
  let text = masked.as_ref();
  for pattern in PATTERNS.iter() {
    let Some(found) = pattern.expression.find(text) else { continue };
    let cause = match pattern.shape {
      MatchShape::Fragment => found.as_str(),
      MatchShape::WholeText => text,
      MatchShape::WholeLine => {
        // The class's own `^|\W` guard is part of the match, so the line is read from the word it found.
        let matched = &text[found.start()..found.end()];
        let start = found.start() + matched.len() - matched.trim_start().len();
        let end = start + matched.trim().len();
        containing_line(text, start, end)
      }
    };
    return Some(cause.to_string());
  }
  None
}

fn containing_line(text: &str, start: usize, end: usize) -> &str {
  let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
  let line_end = text[end..].find('\n').map_or(text.len(), |index| end + index);
  text[line_start..line_end].trim()
}

/// The cause of one run, from the error text its newest result event reports, its error-event contents in file order,
/// its stderr tail and its terminal reason. The result text is the first candidate, then the error events newest first,
/// then the stderr tail. When no candidate matches a pattern, the result text names the cause if there is one.
/// Otherwise the newest error event names it, then the stderr tail, and only then the terminal reason.
pub fn extract_run_cause(
  error_texts: &[&str],
  result_error: Option<&str>,
  stderr: &str,
  terminal_reason: Option<&str>,
) -> ExtractedCause {
  let error_materials: Vec<String> = error_texts
    .iter()
    .map(|text| clean(text))
    .filter(|text| !text.is_empty())
    .filter_map(|text| cause_material(&text))
    .collect();
  let result_material = result_error.map(clean).filter(|text| !text.is_empty()).and_then(|text| cause_material(&text));
  for text in result_material.iter().chain(error_materials.iter().rev()) {
    if let Some(line) = match_cause(text) {
      return ExtractedCause::Cause { line, anchor: anchor(text) };
    }
  }
  let stderr_clean = clean(stderr);
  let stderr_material = cause_material(&stderr_clean).unwrap_or_default();
  if !stderr_material.is_empty()
    && let Some(line) = match_cause(&stderr_material)
  {
    return ExtractedCause::Cause { line, anchor: anchor(&stderr_material) };
  }
  let mut parts: Vec<&str> = error_texts.to_vec();
  parts.extend(result_error);
  parts.push(stderr);
  let combined = parts.join("\n");
  if banner_only(&combined) {
    return ExtractedCause::NoCause { shape: NoCauseShape::BannerOnly, anchor: anchor(combined.trim()) };
  }
  if let Some(text) = result_material.as_ref().or(error_materials.last()) {
    return ExtractedCause::Cause { line: text.clone(), anchor: anchor(text) };
  }
  if !stderr_material.is_empty() {
    return ExtractedCause::Cause { line: stderr_material.clone(), anchor: anchor(&stderr_material) };
  }
  if let Some(reason) = terminal_reason {
    let line = format!("terminal_reason={reason}");
    if has_http_status(reason) {
      return ExtractedCause::Cause { line: line.clone(), anchor: anchor(&line) };
    }
    return ExtractedCause::NoCause { shape: NoCauseShape::TerminalReason, anchor: anchor(&line) };
  }
  if !error_texts.is_empty() || result_error.is_some() || !stderr.is_empty() {
    return ExtractedCause::NoCause { shape: NoCauseShape::EmptyText, anchor: String::new() };
  }
  ExtractedCause::NoCause { shape: NoCauseShape::ExitCodeOnly, anchor: String::new() }
}

/// The exit-code text of a signature: the number, or `null` when the record finished without one.
pub fn exit_text(exit_code: Option<i32>) -> String {
  match exit_code {
    Some(code) => code.to_string(),
    None => "null".to_string(),
  }
}

/// The signature of a run record.
pub fn run_signature(kind: &str, exit_code: Option<i32>, backend: &str, cause: &ExtractedCause) -> String {
  let exit = exit_text(exit_code);
  match cause {
    ExtractedCause::Cause { line, .. } => format!("run:{kind}:{exit}:{}", normalize(line)),
    ExtractedCause::NoCause { .. } => format!("run:{kind}:{exit}:no-cause:{backend}"),
  }
}

/// The signature of one server-log error line.
pub fn log_signature(event: &str, message: &str) -> String {
  format!("log:{event}:{}", normalize(message))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn cause(line: &str) -> ExtractedCause {
    ExtractedCause::Cause { line: line.to_string(), anchor: line.to_string() }
  }

  #[test]
  fn the_two_improve_ux_exit_five_texts_share_one_signature_and_keep_a_cause() {
    let text = "improve runner: the previous round still runs: unit improve-ux.scope exists";
    let first = extract_run_cause(&[text], None, "", None);
    let second = extract_run_cause(&[text], None, "", None);
    assert_eq!(first, second);
    assert_eq!(
      run_signature("work", Some(5), "synthetic-backend", &first),
      "run:work:5:the previous round still runs: unit improve-ux.scope exists"
    );
    assert!(matches!(first, ExtractedCause::Cause { .. }));
  }

  #[test]
  fn a_log_dump_with_no_error_line_yields_to_the_older_error_line() {
    let info = "timestamp=2026-10-18T09:05:08.980Z level=INFO run=568bf1ce message=stream providerID=opencode \
                modelID=deepseek-v4.1-flash-free session.id=ses_eb1cc31f3ffez6qkkXU1c8k14m small=false";
    let failure = "OpenCode backend failed: OpenCode SSE stream carried no session progress";
    let cause = extract_run_cause(&[failure, info], None, "", None);
    assert_eq!(cause, ExtractedCause::Cause { line: failure.to_string(), anchor: failure.to_string() });
    assert_eq!(run_signature("review", Some(-15), "synthetic-backend", &cause), format!("run:review:-15:{failure}"));
  }

  #[test]
  fn a_missing_yaml_frontmatter_skill_load_error_keeps_its_own_signature() {
    let cause = extract_run_cause(
      &[],
      None,
      "Reading additional input from stdin...\n\
       2026-10-19T06:09:24.154864Z ERROR codex_core::session::session: failed to load skill \
       /home/synthetic/.charliebot/skills/zhihu-colony-article/SKILL.md: missing YAML frontmatter delimited by ---",
      None,
    );
    assert_eq!(
      run_signature("work", Some(1), "synthetic-backend", &cause),
      "run:work:1:failed to load skill <path>: missing YAML frontmatter"
    );
  }

  #[test]
  fn the_four_no_cause_shapes_are_closed_and_distinct() {
    let empty = extract_run_cause(&["\u{1b}[0m   "], None, "", None);
    assert_eq!(empty, ExtractedCause::NoCause { shape: NoCauseShape::EmptyText, anchor: String::new() });
    let whitespace = extract_run_cause(&["   \n\t"], None, "", None);
    assert_eq!(whitespace, ExtractedCause::NoCause { shape: NoCauseShape::EmptyText, anchor: String::new() });
    let whitespace_stderr = extract_run_cause(&[], None, "  \n ", None);
    assert_eq!(whitespace_stderr, ExtractedCause::NoCause { shape: NoCauseShape::EmptyText, anchor: String::new() });

    let banner = extract_run_cause(&["Provider List: https://docs.litellm.ai/docs/providers"], None, "", None);
    assert_eq!(
      banner,
      ExtractedCause::NoCause {
        shape: NoCauseShape::BannerOnly,
        anchor: "Provider List: https://docs.litellm.ai/docs/providers".to_string(),
      }
    );

    let terminal = extract_run_cause(&[], None, "", Some("api_error"));
    assert_eq!(
      terminal,
      ExtractedCause::NoCause { shape: NoCauseShape::TerminalReason, anchor: "terminal_reason=api_error".to_string() }
    );

    let exit = extract_run_cause(&[], None, "", None);
    assert_eq!(exit, ExtractedCause::NoCause { shape: NoCauseShape::ExitCodeOnly, anchor: String::new() });

    for (cause, shape) in [
      (empty, NoCauseShape::EmptyText),
      (banner, NoCauseShape::BannerOnly),
      (terminal, NoCauseShape::TerminalReason),
      (exit, NoCauseShape::ExitCodeOnly),
    ] {
      assert!(matches!(cause, ExtractedCause::NoCause { shape: found, .. } if found == shape));
      assert_eq!(run_signature("work", Some(1), "synthetic-backend", &cause), "run:work:1:no-cause:synthetic-backend");
    }
  }

  #[test]
  fn the_colored_three_line_litellm_banner_is_no_cause() {
    let banner = "\u{1b}[1;31mProvider List: https://docs.litellm.ai/docs/providers\u{1b}[0m\n\
                  \u{1b}[1;31mGive Feedback / Get Help: https://github.com/BerriAI/litellm/issues/new\u{1b}[0m\n\
                  \u{1b}[1;31mLiteLLM.Info: If you need to debug this error, use `litellm._turn_on_debug()'.\u{1b}[0m";
    let cause = extract_run_cause(&[banner], None, "", None);
    assert_eq!(cause, ExtractedCause::NoCause { shape: NoCauseShape::BannerOnly, anchor: anchor(banner.trim()) });
  }

  #[test]
  fn a_cut_short_banner_line_is_no_cause() {
    let text = "lm.ai/docs/providers\n\u{1b}[1;31mProvider List: https://docs.litellm.ai/docs/providers\u{1b}[0m";
    let cause = extract_run_cause(&[text], None, "", None);
    assert!(matches!(cause, ExtractedCause::NoCause { shape: NoCauseShape::BannerOnly, .. }), "{cause:?}");
  }

  #[test]
  fn a_single_error_word_takes_its_whole_line() {
    let cause =
      extract_run_cause(&["starting\nError: No deferred tool marker found in the transcript\nrest"], None, "", None);
    let ExtractedCause::Cause { line, .. } = cause else { panic!("{cause:?}") };
    assert_eq!(line, "Error: No deferred tool marker found in the transcript");
  }

  #[test]
  fn a_word_at_the_end_of_a_single_line_text_takes_that_line() {
    let extracted = extract_run_cause(&["the run failed"], None, "", None);
    let ExtractedCause::Cause { line, .. } = extracted else { panic!("{extracted:?}") };
    assert_eq!(line, "the run failed");
  }

  #[test]
  fn a_word_that_merely_contains_oom_is_not_an_oom_kill() {
    let extracted = extract_run_cause(&["the room is out of scope"], None, "", None);
    assert_eq!(extracted, cause("the room is out of scope"));
  }

  #[test]
  fn other_text_is_a_cause_and_a_terminal_reason_with_a_status_is_not_no_cause() {
    let other = extract_run_cause(&["something unexpected happened"], None, "", None);
    assert_eq!(other, cause("something unexpected happened"));
    let status = extract_run_cause(&[], None, "", Some("api_error status=503"));
    assert!(matches!(status, ExtractedCause::Cause { .. }));
  }

  #[test]
  fn a_request_id_that_holds_a_status_digit_run_names_no_cause() {
    let text = "upstream call failed for request 0a401b2c-3d4e-4f50-8a61-7b8c9d0e1f2a";
    let cause = extract_run_cause(&[text], None, "", None);
    assert_eq!(
      run_signature("work", Some(1), "synthetic-backend", &cause),
      "run:work:1:upstream call failed for request <uuid>"
    );
  }

  /// A synthetic result error in the shape a LiteLLM-routed backend reports. The request id is its only per-request
  /// value.
  fn payment_error(request_id: &str) -> String {
    format!(
      "API Error: 402 litellm.BadRequestError: OpenAIException - Insufficient Balance (request_id: {request_id})\n\n\
       LiteLLM: model group 'deepseek-flash' failed with the error above. No fallback was attempted."
    )
  }

  #[test]
  fn result_errors_that_differ_only_in_request_id_share_one_signature() {
    // The first id's hex digits hold `401`, which must not read as an auth failure.
    let first = payment_error("0a401b2c-3d4e-4f50-8a61-7b8c9d0e1f2a");
    let second = payment_error("9c7d6e5f-4a3b-4c2d-8e1f-0a9b8c7d6e5f");
    let first = run_signature("work", Some(1), "synthetic-backend", &extract_run_cause(&[], Some(&first), "", None));
    let second = run_signature("work", Some(1), "synthetic-backend", &extract_run_cause(&[], Some(&second), "", None));
    assert_eq!(first, second);
    assert_eq!(
      first,
      "run:work:1:API Error: <n> litellm.BadRequestError: OpenAIException - Insufficient Balance \
       (request_id: <uuid>) LiteLLM: model group 'deepseek-flash' failed with the error above. \
       No fallback was attempted."
    );
  }

  #[test]
  fn normalization_removes_ids_numbers_timestamps_and_paths() {
    let text = "run 023536a4-48d6-5183-b9f9-8b588ffa0e43 failed at 2026-11-01T20:19:51.151464Z after 12 s in \
                /home/synthetic/.charliebot/sessions/023536a4-48d6-5183-b9f9-8b588ffa0e43/data";
    assert_eq!(normalize(text), "run <uuid> failed at <ts> after <n>s in <path>");
    assert_eq!(normalize("sessions/023536a4-48d6-5183-b9f9-8b588ffa0e43/data"), "<session>/data");
  }

  #[test]
  fn secrets_do_not_reach_a_signature_or_an_anchor() {
    let text = "api_key=abcdefgh12345678 Authorization: Bearer abcdefgh12345 sk-abcdefgh12345";
    let redacted = redact(text);
    assert!(!redacted.contains("abcdefgh"));
    assert!(redacted.contains("<redacted>"));
  }
}
