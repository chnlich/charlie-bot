//! The text scan: the words of a text file, read line by line against the match definition.
//!
//! A word is a run of name, path and flag characters. The scan reads the positions that select a backend from the words
//! around a word: the program after a backend CLI name, the value after a `--backend` flag or a `backend` key, and the
//! operand after a default operator. A code file (a shell, Python, Perl, awk or JavaScript file) is read as code with its
//! comments left out, since a comment chooses nothing that runs. Every other text file is prose. A prose sentence that
//! names a backend CLI describes a harness, so the name is no site there. A model id, an option id, a pool id, a default
//! operand and a gateway URL are sites in prose and in code alike.

use std::path::Path;

use charliebot_core::config;

use crate::rules::{self, CALL_MARKERS, Class, Found, fixed_command};
use crate::vocabulary::Vocabulary;

/// The comment syntax of a code file: its line comment marker, and the delimiters of its block comments when it has any.
#[derive(Clone, Copy)]
struct Syntax {
  line: &'static str,
  block: Option<(&'static str, &'static str)>,
}

/// Shell, Python, Perl and awk: a `#` comment runs to the end of the line.
const HASH: Syntax = Syntax { line: "#", block: None };

/// JavaScript and TypeScript: a `//` comment runs to the end of the line, and a `/* ... */` comment may span lines.
const SLASH: Syntax = Syntax { line: "//", block: Some(("/*", "*/")) };

/// The code file extensions, each with its comment syntax.
const CODE_EXTENSIONS: [(&str, Syntax); 9] = [
  ("sh", HASH),
  ("bash", HASH),
  ("py", HASH),
  ("pl", HASH),
  ("awk", HASH),
  ("mjs", SLASH),
  ("cjs", SLASH),
  ("js", SLASH),
  ("ts", SLASH),
];

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
  /// Prose: a backend CLI name in a sentence describes a harness, so it is no site. The other classes still are.
  Prose,
}

/// The words of a line, each with its byte offsets and its text with the surrounding dots and slashes trimmed.
struct Word<'a> {
  start: usize,
  end: usize,
  text: &'a str,
}

/// Scan a whole text file. A code file's code is read as code, with its comments left out; any other text file is prose.
pub fn scan_file(vocab: &Vocabulary, text: &str, path: &Path) -> Vec<Found> {
  let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or_default();
  let code = CODE_EXTENSIONS.iter().find(|(name, _)| *name == ext).map(|(_, syntax)| *syntax);
  let mut found = Vec::new();
  // The closing delimiter of the block comment that the previous line left open, if any.
  let mut open_block = None;
  for (index, line) in text.lines().enumerate() {
    let number = index + 1;
    match code {
      Some(syntax) => {
        let code_text = strip_comments(line, syntax, &mut open_block);
        scan_line(vocab, &code_text, number, Mode::Code, &mut found);
      }
      None => scan_line(vocab, line, number, Mode::Prose, &mut found),
    }
  }
  found
}

/// The code of a line, with each comment blanked to a space so that the words on either side of it stay apart. A line
/// comment starts at its marker when the line's start or a whitespace comes before the marker, and a block comment at its
/// opening delimiter. Neither opens inside a quoted string. A block comment that the line does not close leaves its
/// closing delimiter in `open_block` for the next line.
fn strip_comments(line: &str, syntax: Syntax, open_block: &mut Option<&'static str>) -> String {
  let mut code = String::with_capacity(line.len());
  let mut quote: Option<char> = None;
  let mut resume = 0;
  for (at, ch) in line.char_indices() {
    if at < resume {
      continue;
    }
    if let Some(close) = *open_block {
      let Some(end) = line[at..].find(close) else { break };
      *open_block = None;
      resume = at + end + close.len();
      code.push(' ');
      continue;
    }
    if let Some(open) = quote {
      if ch == open {
        quote = None;
      }
      code.push(ch);
      continue;
    }
    let rest = &line[at..];
    if matches!(ch, '"' | '\'') {
      quote = Some(ch);
    } else if rest.starts_with(syntax.line) && code.chars().last().is_none_or(char::is_whitespace) {
      break;
    } else if let Some((block_open, block_close)) = syntax.block
      && rest.starts_with(block_open)
    {
      *open_block = Some(block_close);
      resume = at + block_open.len();
      code.push(' ');
      continue;
    }
    code.push(ch);
  }
  code
}

/// Scan a text block whose first line is `first_line`, such as the value of a Rust string literal, as prose.
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
    // A shell default glues its operator's dash to the value, as `:-openai/glm` in `${MODEL:-openai/glm}`: the word
    // carries the dash, the value it names does not. A flag keeps its dashes for the positions below.
    let value = word.text.trim_start_matches('-');
    if let Some(class) = rules::value_class(vocab, value) {
      found.push(site(number, class, value));
    }
    let Some(next) = words.get(index + 1) else { continue };
    let gap = &line[word.end..next.start];
    if mode == Mode::Code
      && let Some(command) = fixed_command(word.text)
      && only(gap, COMMAND_GAP)
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
/// unquoted, as in `${VAR:-operand}`.
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

  /// Prose names a harness and its call, in a backtick or not, and the name is no site. A model id in the same prose is.
  #[test]
  fn prose_names_a_harness_without_a_site() {
    let found = classes("run `claude -p --resume x` now\nclaude -p in prose\nuse `claude --version`\n", "guide.md");
    assert!(found.is_empty(), "{found:?}");
    let model = classes("run `claude -p --model claude-opus-5-5` now\n", "guide.md");
    assert_eq!(model, vec![(1, Class::ModelId, "claude-opus-5-5".to_string())]);
  }

  /// A comment chooses nothing that runs, so a code file's comment is no site, whatever it names. A marker inside a
  /// quoted string opens no comment, so the call after that string is code.
  #[test]
  fn a_code_comment_is_no_site_and_a_quoted_marker_is_no_comment() {
    let found = classes(
      "# claude -p reads stdin\necho \"a # b\" && claude -p\n# option claude-opus, model claude-opus-5-5\n",
      "run.sh",
    );
    assert_eq!(found, vec![(2, Class::FixedCommand, "claude -p".to_string())]);
  }

  /// A block comment in a JavaScript file is no site, across its lines, and the code after its close is read.
  #[test]
  fn a_block_comment_is_no_site_and_the_code_after_it_is_read() {
    let found = classes("/* default\n   claude-opus-5-5 */ const x = \"claude-opus\";\n", "run.mjs");
    assert_eq!(found, vec![(2, Class::OptionId, "claude-opus".to_string())]);
  }

  /// A gateway URL in a JavaScript line comment is no site, and the `//` of the comment is not part of any URL. The
  /// same URL in a string of the code is a site.
  #[test]
  fn a_gateway_url_in_a_comment_is_no_site_and_in_code_it_is() {
    let found = classes(
      "const x = 1; // see https://aigw.example.team/v1\nconst y = \"https://aigw.example.team/v1\";\n",
      "run.mjs",
    );
    assert_eq!(found, vec![(2, Class::Gateway, "https://aigw.example.team/v1".to_string())]);
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

  /// A shell default's operand is the value without the `:-` operator's dash, and a flag keeps its dashes.
  #[test]
  fn an_operator_dash_stays_off_the_reported_value() {
    let found = classes("MODEL=${RELEASE_LAB_MODEL:-openai/glm-5.3-flash}\n", "run.sh");
    assert!(found.contains(&(1, Class::DefaultValue, "openai/glm-5.3-flash".to_string())), "{found:?}");
    assert!(found.iter().all(|(_, _, value)| !value.starts_with('-')), "{found:?}");
    let flags = classes("charlie --backend cheap\n", "run.sh");
    assert_eq!(flags, vec![(1, Class::PoolId, "cheap".to_string())]);
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
