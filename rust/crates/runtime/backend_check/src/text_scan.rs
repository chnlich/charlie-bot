//! The text scan: the words of a text file, read line by line against the match definition.
//!
//! A word is a run of name, path and flag characters. The scan reads the positions that select a backend from the words
//! around a word: the program after a backend CLI name, the value after a `--backend` flag or a `backend` key, and the
//! operand after a default operator. What a position means depends on the kind of text. A code file (a shell, Python or
//! script file) runs a backend CLI wherever its name is followed by a call marker. Protocol text (a document, a prompt,
//! a Rust string literal) names a backend CLI only in a backtick command, so English prose that says `run` after a
//! word stays clean.

use std::path::Path;

use charliebot_core::config;

use crate::rules::{self, CALL_MARKERS, Class, Found, fixed_command};
use crate::vocabulary::Vocabulary;

/// The extensions of code files.
const CODE_EXTENSIONS: [&str; 9] = ["sh", "bash", "py", "mjs", "cjs", "js", "ts", "pl", "awk"];

/// The characters between a backend CLI name and its call marker: spaces, quotes and brackets, never a letter or `:`.
const COMMAND_GAP: &str = " \t,'\"`[](){}";

/// The characters between a `--backend` flag and its value, or a `backend` key and its value.
const FLAG_GAP: &str = " =\"'`\t";
const KEY_GAP: &str = " :=\"'`\t";

/// The kind of text a line is read as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
  /// A code file: a backend CLI runs wherever a call marker follows its name, and the code may read `config.yaml`.
  Code,
  /// Protocol text: a backend CLI counts only inside a backtick command.
  Prose,
}

/// The words of a line, each with its byte offsets and its text with the surrounding dots and slashes trimmed.
struct Word<'a> {
  start: usize,
  end: usize,
  text: &'a str,
}

/// Scan a whole text file. A code file is read as code, any other text file as protocol text.
pub fn scan_file(vocab: &Vocabulary, text: &str, path: &Path) -> Vec<Found> {
  let is_code = path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| CODE_EXTENSIONS.contains(&ext));
  let mode = if is_code { Mode::Code } else { Mode::Prose };
  let mut found = Vec::new();
  for (index, line) in text.lines().enumerate() {
    scan_line(vocab, line, index + 1, mode, &mut found);
  }
  found
}

/// Scan a text block whose first line is `first_line`, such as the value of a Rust string literal, as protocol text.
pub fn scan_block(vocab: &Vocabulary, text: &str, first_line: usize) -> Vec<Found> {
  let mut found = Vec::new();
  for (index, line) in text.lines().enumerate() {
    scan_line(vocab, line, first_line + index, Mode::Prose, &mut found);
  }
  found
}

fn scan_line(vocab: &Vocabulary, line: &str, number: usize, mode: Mode, found: &mut Vec<Found>) {
  let words = words(line);
  for (index, word) in words.iter().enumerate() {
    if let Some(class) = rules::value_class(vocab, word.text) {
      found.push(site(number, class, word.text));
    }
    let Some(next) = words.get(index + 1) else { continue };
    let gap = &line[word.end..next.start];
    if let Some(command) = fixed_command(word.text)
      && runs_as_command(mode, line, word, gap)
      && CALL_MARKERS.contains(&next.text)
    {
      found.push(site(number, Class::FixedCommand, &format!("{command} {}", next.text)));
    }
    if follows_pool_position(word.text, gap) && vocab.pools.contains(next.text) {
      found.push(site(number, Class::PoolId, next.text));
    }
  }
  for operand in default_operands(mode, line) {
    if rules::names_backend(vocab, operand) {
      found.push(site(number, Class::DefaultValue, operand));
    }
  }
  for url in gateway_urls(vocab, line) {
    found.push(site(number, Class::Gateway, url));
  }
  if mode == Mode::Code && line.contains(config::CONFIG_FILENAME) && words.iter().any(|word| word.text == "backends") {
    found.push(site(number, Class::ConfigBypass, "backends"));
  }
}

/// Whether a backend CLI name at `word` runs a program here: a code line runs it when the gap before the marker holds
/// only separators, and protocol text runs it only inside a backtick.
fn runs_as_command(mode: Mode, line: &str, word: &Word<'_>, gap: &str) -> bool {
  match mode {
    Mode::Code => only(gap, COMMAND_GAP),
    Mode::Prose => line[..word.start].ends_with('`') && only(gap, COMMAND_GAP),
  }
}

fn site(line: usize, class: Class, value: &str) -> Found {
  Found { line, class, value: value.to_string() }
}

/// The words of a line.
fn words(line: &str) -> Vec<Word<'_>> {
  let mut words = Vec::new();
  let mut start = None;
  for (index, ch) in line.char_indices() {
    if is_word_char(ch) {
      start.get_or_insert(index);
    } else if let Some(begin) = start.take() {
      words.push(word(line, begin, index));
    }
  }
  if let Some(begin) = start {
    words.push(word(line, begin, line.len()));
  }
  words
}

fn word(line: &str, start: usize, end: usize) -> Word<'_> {
  Word { start, end, text: line[start..end].trim_matches(['.', '/']) }
}

fn is_word_char(ch: char) -> bool {
  ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | '~' | '$')
}

/// Whether a word followed by `gap` puts the next word in a backend position: a `--backend` flag, or a `backend` key
/// with a separator that holds a `:` or `=`.
fn follows_pool_position(word: &str, gap: &str) -> bool {
  (word == "--backend" && only(gap, FLAG_GAP)) || (word == "backend" && only(gap, KEY_GAP) && gap.contains([':', '=']))
}

/// Whether every character of `gap` is one of `allowed`.
fn only(gap: &str, allowed: &str) -> bool {
  gap.chars().all(|ch| allowed.contains(ch))
}

/// The single-word operands after a default operator, and the defaults of a Python `def`. Code reads every operator;
/// protocol text reads only the symbol operators, since ` or ` is an English word. A shell `:-` operand may be
/// unquoted, as in `${VAR:-claude-opus}`.
fn default_operands(mode: Mode, line: &str) -> Vec<&str> {
  let operators: &[&str] = match mode {
    Mode::Code => &["??", "||", ":-", " or "],
    Mode::Prose => &["??", "||", ":-"],
  };
  let mut operands = Vec::new();
  for operator in operators {
    for (at, _) in line.match_indices(operator) {
      let rest = line[at + operator.len()..].trim_start_matches([' ', '\t', '(', '|', '?', '-']);
      let unquoted = (*operator == ":-").then(|| leading_word(rest)).flatten();
      operands.extend(quoted_word(rest).or(unquoted));
    }
  }
  if mode == Mode::Code && line.trim_start().starts_with("def ") {
    for (at, _) in line.match_indices('=') {
      operands.extend(quoted_word(&line[at + 1..]));
    }
  }
  operands
}

/// The text of a quoted word at the start of `text`, when it is one word.
fn quoted_word(text: &str) -> Option<&str> {
  let quote = text.chars().next().filter(|ch| matches!(ch, '"' | '\'' | '`'))?;
  let inner = &text[1..];
  let word = &inner[..inner.find(quote)?];
  (!word.is_empty() && !word.contains(char::is_whitespace)).then_some(word)
}

/// The word at the start of `text`, when `text` starts with one.
fn leading_word(text: &str) -> Option<&str> {
  let end = text.find(|ch: char| !is_word_char(ch)).unwrap_or(text.len());
  let word = text[..end].trim_matches(['.', '/']);
  (!word.is_empty()).then_some(word)
}

/// The URLs in a line whose host is a vocabulary host, each up to the first character that ends a URL.
fn gateway_urls<'a>(vocab: &Vocabulary, line: &'a str) -> Vec<&'a str> {
  let mut urls = Vec::new();
  for scheme in ["https://", "http://"] {
    for (at, _) in line.match_indices(scheme) {
      let rest = &line[at + scheme.len()..];
      let host: String = rest.chars().take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-')).collect();
      if vocab.hosts.contains(&host.to_ascii_lowercase()) {
        let end =
          line[at..].find(|ch: char| ch.is_whitespace() || "\"'`)>],".contains(ch)).map_or(line.len(), |n| at + n);
        urls.push(&line[at..end]);
      }
    }
  }
  urls
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;
  use std::path::PathBuf;

  use super::*;

  fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| item.to_string()).collect()
  }

  fn vocab() -> Vocabulary {
    Vocabulary {
      options: set(&["claude-opus"]),
      pools: set(&["cheap"]),
      models: set(&[]),
      hosts: set(&["aigw.example.team"]),
      id_prefixes: set(&["claude-"]),
    }
  }

  fn classes(text: &str, path: &str) -> Vec<(usize, Class, String)> {
    scan_file(&vocab(), text, &PathBuf::from(path)).into_iter().map(|f| (f.line, f.class, f.value)).collect()
  }

  /// In protocol text a backend CLI with a call marker is a fixed command only inside a backtick; the same words in
  /// prose, or a flag that makes no call, are not.
  #[test]
  fn protocol_text_names_a_command_only_inside_a_backtick() {
    let found = classes("run `claude -p --resume x` now\nclaude -p in prose\nuse `claude --version`\n", "guide.md");
    assert_eq!(found, vec![(1, Class::FixedCommand, "claude -p".to_string())]);
  }

  /// In a code file a backend CLI with a call marker is a fixed command wherever it stands, as in an argv list.
  #[test]
  fn code_runs_a_command_wherever_a_call_marker_follows() {
    let found = classes("run.sh: claude -p --resume x\nargv = [\"claude\", \"-p\"]\nclaude --version\n", "run.sh");
    assert_eq!(
      found,
      vec![(1, Class::FixedCommand, "claude -p".to_string()), (2, Class::FixedCommand, "claude -p".to_string())]
    );
  }

  /// A pool id names a backend only after a `--backend` flag or a `backend` key; the same word in prose names none.
  #[test]
  fn a_pool_id_names_a_backend_only_in_a_selecting_position() {
    let found = classes("charlie --backend cheap\n\"backend\": \"cheap\"\nthe cheap pool\n", "x.sh");
    assert_eq!(found, vec![(1, Class::PoolId, "cheap".to_string()), (2, Class::PoolId, "cheap".to_string())]);
  }

  /// A default operand that names a backend is a default-value site, in the shell, Python and `??` forms.
  #[test]
  fn a_default_operand_naming_a_backend_is_a_default_value() {
    let shell = classes("model=\"${MODEL:-claude-opus}\"\n", "run.sh");
    assert!(shell.contains(&(1, Class::DefaultValue, "claude-opus".to_string())), "{shell:?}");
    let or_form = classes("x = os.get(k) or \"claude\"\n", "run.py");
    assert!(or_form.contains(&(1, Class::DefaultValue, "claude".to_string())), "{or_form:?}");
    let python = classes("def run(model=\"claude-opus\"):\n", "run.py");
    assert!(python.contains(&(1, Class::DefaultValue, "claude-opus".to_string())), "{python:?}");
    let nullish = classes("const m = cfg.model ?? \"cheap\";\n", "run.mjs");
    assert!(nullish.contains(&(1, Class::DefaultValue, "cheap".to_string())), "{nullish:?}");
    let prose = classes("pick a cheap or claude model\n", "guide.md");
    assert!(prose.iter().all(|(_, class, _)| *class != Class::DefaultValue), "{prose:?}");
  }

  /// A gateway URL is a site when its host is a vocabulary host.
  #[test]
  fn a_gateway_url_is_a_site_when_its_host_is_known() {
    let found = classes("api https://aigw.example.team/v1 and https://elsewhere.example/v1\n", "doc.md");
    assert_eq!(found, vec![(1, Class::Gateway, "https://aigw.example.team/v1".to_string())]);
  }

  /// A code line that reads `config.yaml` and names `backends` reads the configuration itself. A line that writes a
  /// `backends:` block, or prose that names both, is not a read.
  #[test]
  fn a_code_line_reading_the_backends_section_is_a_bypass() {
    let code = classes("awk '/backends:/' config.yaml\n", "remote.sh");
    assert_eq!(code, vec![(1, Class::ConfigBypass, "backends".to_string())]);
    assert!(classes("cat > config.yaml <<EOF\nbackends:\nEOF\n", "trial.sh").is_empty());
    assert!(classes("read config.yaml and its backends\n", "notes.md").is_empty());
  }
}
