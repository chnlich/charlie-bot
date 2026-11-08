//! The shell text the scan-guard rules read. Heredoc bodies are removed and `bash -c` strings are unwrapped. Quoted
//! text keeps the meaning of its bare form. Each relative path word resolves to the absolute path it names, against
//! the hook's cwd and the latest `cd` before it.

use std::sync::OnceLock;

use charliebot_core::config::SESSIONS_DIR_NAME;
use charliebot_core::home::DEFAULT_HOME_DIR_NAME;
use regex::{Captures, Regex};

/// Replaces a `|`, `;` or `&` inside double quotes, so the quoted character is not a shell separator.
const QUOTED_SEPARATOR: char = '\u{1}';

/// A `cd` command's byte span, and the directory it moves to: `None` for a bare `cd`, which goes home.
type CdSpan = (usize, usize, Option<String>);

/// The command with heredoc bodies removed, `bash -c` strings unwrapped, and quoted spans reduced. A single-quoted
/// string becomes its text when it is one argument. It is dropped when it holds a space or a separator, because that
/// is script or prose rather than shell. A double-quoted string keeps its text with its separators neutralized, unless
/// it spans lines or holds a command substitution or backtick.
pub fn shell_text(command: &str) -> String {
  let without_heredocs = if command.contains("<<") { mask_heredocs(command) } else { command.to_string() };
  let unwrapped = if without_heredocs.contains("-c") {
    bash_c_pattern()
      .replace_all(&without_heredocs, |captures: &Captures<'_>| {
        captures.get(1).or_else(|| captures.get(2)).map_or_else(String::new, |text| text.as_str().to_string())
      })
      .into_owned()
  } else {
    without_heredocs
  };
  if !unwrapped.contains('"') && !unwrapped.contains('\'') {
    return unwrapped;
  }
  quoted_span_pattern()
    .replace_all(&unwrapped, |captures: &Captures<'_>| {
      let span = captures.get(0).expect("group 0 is the whole match").as_str();
      if span.starts_with('"') {
        neutralize_double_quoted(span)
      } else {
        captures.get(1).map_or_else(String::new, |argument| argument.as_str().to_string())
      }
    })
    .into_owned()
}

fn neutralize_double_quoted(span: &str) -> String {
  if span.contains(['\n', '`']) || span.contains("$(") {
    return span.to_string();
  }
  span.replace(['|', ';', '&'], &QUOTED_SEPARATOR.to_string())
}

/// The command with each relative path word replaced by the absolute path it names. A word resolves against the hook's
/// cwd, or against the latest `cd` before it. A bare word (no slash) resolves only when `store_root` matches it in
/// full, which is the session store or a parent of it. Options, assignments, URLs and wildcards other than `*/...` stay
/// as written.
pub fn resolve_paths(shell: &str, cwd: &str, store_root: &Regex) -> String {
  if !needs_path_resolution(shell) {
    return shell.to_string();
  }
  let cds = cd_spans(shell);
  let mut out = String::new();
  let mut last = 0;
  let mut current = cwd.to_string();
  let mut next_cd = 0;
  for (start, end) in word_spans(shell) {
    while let Some((cd_start, cd_end, target)) = cds.get(next_cd) {
      if *cd_start >= start || start < *cd_end {
        break;
      }
      current = match target {
        None => "~".to_string(),
        Some(path) => join_path(&current, path),
      };
      next_cd += 1;
    }
    if cds.iter().any(|(cd_start, cd_end, _)| *cd_start <= start && start < *cd_end) {
      continue;
    }
    let word = &shell[start..end];
    let Some(resolved) = resolve_word(&strip_quotes(word), &current, store_root) else {
      continue;
    };
    if resolved == word {
      continue;
    }
    out.push_str(&shell[last..start]);
    out.push_str(&resolved);
    last = end;
  }
  out.push_str(&shell[last..]);
  out
}

pub(super) fn needs_path_resolution(shell: &str) -> bool {
  shell.split(|character: char| character.is_whitespace() || ";&|()<>`".contains(character)).any(|word| {
    if word == "cd" {
      return true;
    }
    let bare = word.trim_matches(['"', '\'']);
    if bare.starts_with(['-', '?', '[', '!']) || bare.contains('=') || bare.contains("://") {
      return false;
    }
    if bare.starts_with(['~', '/']) || (bare.starts_with('$') && bare.contains('/')) {
      return false;
    }
    if bare.starts_with('*') && !bare.starts_with("*/") {
      return false;
    }
    bare.contains('/') || matches!(bare, "." | ".." | DEFAULT_HOME_DIR_NAME | SESSIONS_DIR_NAME)
  })
}

/// A word without its quotes, resolved against the current directory, or `None` when the word names no path.
fn resolve_word(bare: &str, current: &str, store_root: &Regex) -> Option<String> {
  if bare.starts_with(['-', '?', '[', '!']) || bare.contains('=') || bare.contains("://") {
    return None;
  }
  if bare.starts_with('*') && !bare.starts_with("*/") {
    return None;
  }
  if bare.starts_with(['~', '/']) || (bare.starts_with('$') && bare.contains('/')) {
    return Some(bare.to_string());
  }
  let joined = join_path(current, bare);
  if bare.contains('/') || bare == "." || bare == ".." {
    return Some(joined);
  }
  store_root.is_match(&joined).then_some(joined)
}

/// Joins a relative path onto a base. An absolute path, a `~` path or a `$VAR` path stands on its own.
fn join_path(base: &str, relative: &str) -> String {
  if relative.starts_with(['/', '~', '$']) {
    return normalize_path(relative);
  }
  normalize_path(&format!("{}/{relative}", base.trim_end_matches('/')))
}

/// The path with `.` and `..` collapsed. A leading `/` is the root; a leading `~` or `$VAR` segment stays as written.
fn normalize_path(path: &str) -> String {
  let (head, rest) = match path.strip_prefix('/') {
    Some(rest) => ("/", rest),
    None => path.split_once('/').unwrap_or((path, "")),
  };
  let mut segments: Vec<&str> = Vec::new();
  for segment in rest.split('/') {
    match segment {
      "" | "." => {}
      ".." if !segments.is_empty() => {
        segments.pop();
      }
      _ => segments.push(segment),
    }
  }
  let joined = segments.join("/");
  if head == "/" {
    format!("/{joined}")
  } else if joined.is_empty() {
    head.to_string()
  } else {
    format!("{head}/{joined}")
  }
}

/// A word with its quote characters removed: the text it means as a bare argument.
pub fn strip_quotes(word: &str) -> String {
  word.chars().filter(|character| !matches!(character, '"' | '\'')).collect()
}

/// The shell words of a text, as byte spans.
pub fn word_spans(shell: &str) -> Vec<(usize, usize)> {
  word_pattern().find_iter(shell).map(|word| (word.start(), word.end())).collect()
}

fn cd_spans(shell: &str) -> Vec<CdSpan> {
  cd_pattern()
    .captures_iter(shell)
    .map(|captures| {
      let whole = captures.get(0).expect("group 0 is the whole match");
      let target = captures.get(1).map(|path| strip_quotes(path.as_str()));
      (whole.start(), whole.end(), target)
    })
    .collect()
}

pub fn parse_error(error: regex::Error) -> String {
  format!("scan command parsing failed: {error}")
}

fn word_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"[^\s;&|()<>`]+").expect("word pattern is valid"))
}

/// A `cd` at the start of a command, with its one argument: a sequence of quoted and bare chunks.
fn cd_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| {
    Regex::new(r#"(?:[;&|(\n`]|^)[ \t]*cd(?:[ \t]+((?:"[^"\n]*"|'[^'\n]*'|[^\s;&|()<>`'"])+)|[ \t]*(?:[;&|()\n`]|$))"#)
      .expect("cd pattern is valid")
  })
}

/// A `<<EOF` heredoc opener, capturing the delimiter word its closing line starts with.
fn heredoc_opener_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r#"<<-?\s*['"]?(\w+)['"]?"#).expect("heredoc opener pattern is valid"))
}

/// The command with each heredoc body dropped: the `<<EOF` opener stays, and the text from the following line
/// through the line that opens with the delimiter goes. An opener with no closing line keeps its text.
fn mask_heredocs(command: &str) -> String {
  let mut out = String::with_capacity(command.len());
  let mut last = 0;
  for captures in heredoc_opener_pattern().captures_iter(command) {
    let opener = captures.get(0).expect("group 0 is the whole match");
    if opener.start() < last {
      continue;
    }
    let delimiter = captures.get(1).expect("the opener pattern carries the delimiter group").as_str();
    let Some(line_end) = command[opener.end()..].find('\n').map(|at| opener.end() + at) else {
      break;
    };
    let Some(close_end) = heredoc_close(command, line_end + 1, delimiter) else { continue };
    out.push_str(&command[last..opener.start()]);
    out.push_str("<<HEREDOC");
    last = close_end;
  }
  out.push_str(&command[last..]);
  out
}

/// The end of the first line after `from` that opens with `delimiter` alone, or `None` when none does.
fn heredoc_close(command: &str, from: usize, delimiter: &str) -> Option<usize> {
  let closing = format!("\n{delimiter}");
  let mut search_from = from;
  while let Some(end) = command[search_from..].find(&closing).map(|at| search_from + at + closing.len()) {
    if command[end..].chars().next().is_none_or(|next| !next.is_alphanumeric() && next != '_') {
      return Some(end);
    }
    search_from = end;
  }
  None
}

fn bash_c_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| {
    Regex::new(r#"\b(?:bash|sh|zsh)\s+-[lc]*c\s+(?:'([^'\n]*)'|"((?:[^"\\\n]|\\.)*)")"#)
      .expect("bash -c pattern is valid")
  })
}

/// A quoted span: a double-quoted string with its escapes, a single-quoted string of one argument (group 1), or any
/// other single-quoted string.
fn quoted_span_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| {
    Regex::new(r#"(?s)"(?:[^"\\]|\\.)*"|'([^'\s;|&`]*)'|'[^']*'"#).expect("quoted-span pattern is valid")
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn double_quoted_apostrophe_does_not_open_a_single_quote() {
    assert_eq!(shell_text(r#"echo "session's note"; find ~/x -name y"#), r#"echo "session's note"; find ~/x -name y"#);
  }

  #[test]
  fn double_quoted_separator_is_not_a_pipe() {
    assert!(!shell_text(r#"grep -rn "a\|b" ~/x"#).contains('|'));
  }

  #[test]
  fn relative_paths_resolve_against_the_cwd_and_the_latest_cd() {
    let store_root = Regex::new(r"^/home/a/\.charliebot/sessions$").expect("test pattern is valid");
    let resolved = resolve_paths("cd .. && find . -name X", "/home/a/.charliebot/sessions/abc", &store_root);
    assert_eq!(resolved, "cd .. && find /home/a/.charliebot/sessions -name X");
    let resolved = resolve_paths("find ../../../ -name X", "/home/a/b/c", &store_root);
    assert_eq!(resolved, "find /home -name X");
  }

  #[test]
  fn a_variable_word_keeps_the_quotes_around_it() {
    let store_root = Regex::new(r"^/home/a/\.charliebot/sessions$").expect("test pattern is valid");
    let resolved = resolve_paths(r#"echo "== $f"; find . -name X"#, "/home/a/.charliebot/sessions", &store_root);
    assert_eq!(resolved, r#"echo "== $f"; find /home/a/.charliebot/sessions -name X"#);
  }
}
