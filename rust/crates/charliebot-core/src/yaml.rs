//! The `yaml_utils.save_yaml` writer (`src/infra/yaml_utils.py`).
//!
//! The bytes match PyYAML 6's safe C dumper with `allow_unicode`, block style and `sort_keys=False`. Two parts of
//! PyYAML decide them: `yaml/resolver.py` says which plain strings would read back as another tag, and
//! `yaml/emitter.py` folds plain and single-quoted scalars at column 80.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

/// PyYAML's `Emitter.best_width`.
const BEST_WIDTH: usize = 80;

/// Dump a mapping or a sequence, the shapes that cron files hold.
pub fn dump(value: &Value) -> Result<String, serde_yaml::Error> {
  let mut output = String::new();
  write_item(&mut output, value, 0);
  Ok(output)
}

/// Writes a mapping or a sequence whose first line starts at the cursor. The cursor is at column `indent` after a
/// `- ` marker, or at column 0 for the document root.
fn write_item(output: &mut String, value: &Value, indent: usize) {
  match value {
    Value::Object(map) if map.is_empty() => output.push_str("{}\n"),
    Value::Array(items) if items.is_empty() => output.push_str("[]\n"),
    Value::Object(map) => write_mapping(output, map, indent),
    Value::Array(items) => write_sequence(output, items, indent),
    scalar => {
      write_scalar(output, scalar, indent);
      output.push('\n');
    }
  }
}

/// Writes each entry on its own line, with its key at column `indent`.
fn write_mapping(output: &mut String, map: &Map<String, Value>, indent: usize) {
  for (key, value) in map {
    pad_to(output, indent);
    write_pair(output, key, value, indent);
  }
}

/// Writes each item on its own line, with its `- ` marker at column `indent`.
fn write_sequence(output: &mut String, items: &[Value], indent: usize) {
  for item in items {
    pad_to(output, indent);
    output.push_str("- ");
    write_item(output, item, indent + 2);
  }
}

/// Writes `key:` and its value, with the key at column `indent`. A nested mapping is indented by two, while a sequence
/// under a key stays at `indent`, because PyYAML leaves it unindented. A scalar's folded lines continue at
/// `indent + 2`.
fn write_pair(output: &mut String, key: &str, value: &Value, indent: usize) {
  write_string(output, key, true, indent);
  output.push(':');
  match value {
    Value::Object(map) if map.is_empty() => output.push_str(" {}\n"),
    Value::Array(items) if items.is_empty() => output.push_str(" []\n"),
    Value::Object(map) => {
      output.push('\n');
      write_mapping(output, map, indent + 2);
    }
    Value::Array(items) => {
      output.push('\n');
      write_sequence(output, items, indent);
    }
    scalar => {
      output.push(' ');
      write_scalar(output, scalar, indent + 2);
      output.push('\n');
    }
  }
}

/// Writes a scalar. Its folded lines continue at `indent`.
fn write_scalar(output: &mut String, value: &Value, indent: usize) {
  match value {
    Value::Null => output.push_str("null"),
    Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
    Value::Number(value) => output.push_str(&value.to_string()),
    Value::String(value) => write_string(output, value, false, indent),
    Value::Array(_) | Value::Object(_) => unreachable!("write_item and write_pair write containers"),
  }
}

/// Writes a string in the style that PyYAML's emitter picks: plain when the text reads back as a string, single-quoted
/// when the text allows it, and otherwise double-quoted with JSON escapes, which YAML reads back as the same text.
fn write_string(output: &mut String, value: &str, key: bool, indent: usize) {
  if plain_safe(value) {
    write_plain(output, value, indent, !key);
  } else if single_quoted_allowed(value, key) {
    write_single_quoted(output, value, indent, !key);
  } else {
    output.push_str(&serde_json::to_string(value).expect("a string serializes as JSON"));
  }
}

/// PyYAML's `write_plain`. Plain text holds no line break, so a fold happens only at a single space, and only once the
/// line is past `BEST_WIDTH`. A key passes `split` as false, because PyYAML never folds a simple key.
fn write_plain(output: &mut String, text: &str, indent: usize, split: bool) {
  let chars: Vec<char> = text.chars().collect();
  let mut line = Line::new(output, indent);
  let mut word_start = 0;
  while word_start < chars.len() {
    let word_end = word_start + chars[word_start..].iter().take_while(|&&ch| ch != ' ').count();
    let space_end = word_end + chars[word_end..].iter().take_while(|&&ch| ch == ' ').count();
    line.push(&chars[word_start..word_end]);
    if split && space_end - word_end == 1 && line.column > BEST_WIDTH {
      line.fold();
    } else {
      line.push(&chars[word_end..space_end]);
    }
    word_start = space_end;
  }
}

/// PyYAML's `write_single_quoted`. A single space folds once the line is past `BEST_WIDTH`, except at either end of the
/// text. Each line break in the text becomes an empty line, and each quote is doubled.
fn write_single_quoted(output: &mut String, text: &str, indent: usize, split: bool) {
  let chars: Vec<char> = text.chars().collect();
  let mut line = Line::new(output, indent);
  line.push(&['\'']);
  let mut spaces = false;
  let mut breaks = false;
  let mut start = 0;
  for end in 0..=chars.len() {
    let ch = chars.get(end).copied();
    if spaces {
      if ch != Some(' ') {
        if start + 1 == end && line.column > BEST_WIDTH && split && start != 0 && end != chars.len() {
          line.fold();
        } else {
          line.push(&chars[start..end]);
        }
        start = end;
      }
    } else if breaks {
      if !ch.is_some_and(is_break) {
        if chars[start] == '\n' {
          line.line_break('\n');
        }
        for &br in &chars[start..end] {
          line.line_break(br);
        }
        line.write_indent();
        start = end;
      }
    } else if ch.is_none_or(|ch| ch == ' ' || is_break(ch) || ch == '\'') {
      line.push(&chars[start..end]);
      start = end;
    }
    if ch == Some('\'') {
      line.push_str("''");
      start = end + 1;
    }
    if let Some(ch) = ch {
      spaces = ch == ' ';
      breaks = is_break(ch);
    }
  }
  line.push(&['\'']);
}

/// Whether PyYAML's emitter writes the text plain: no edge or line break, no indicator where a plain scalar cannot
/// start, no `: ` or ` #`, no trailing colon or space, and no implicit tag that would read it back as another type.
fn plain_safe(value: &str) -> bool {
  if value.is_empty() || value.trim() != value || value.contains(['\n', '\r', '\t']) {
    return false;
  }
  if value.starts_with(['-', '?', ':', ','])
    || value.starts_with(['[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
  {
    return false;
  }
  if value.contains(": ") || value.contains(" #") || value.ends_with(':') || value.ends_with(' ') {
    return false;
  }
  implicit_tag(value).is_none()
}

/// Whether PyYAML's `analyze_scalar` leaves single quotes open for the text: no special character, no space before a
/// break and no break before a space. A key with a break is written in double quotes too (`simple_key_context`).
fn single_quoted_allowed(value: &str, key: bool) -> bool {
  let chars: Vec<char> = value.chars().collect();
  let break_beside_space =
    chars.windows(2).any(|pair| (pair[0] == ' ' && is_break(pair[1])) || (is_break(pair[0]) && pair[1] == ' '));
  let multiline = chars.iter().any(|&ch| is_break(ch));
  !chars.iter().any(|&ch| is_special(ch)) && !break_beside_space && !(key && multiline)
}

/// The characters that PyYAML's emitter treats as line breaks.
fn is_break(ch: char) -> bool {
  matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// PyYAML's `special_characters` with `allow_unicode`: the emitter writes such a character only in double quotes.
fn is_special(ch: char) -> bool {
  if ch == '\n' || (' '..='~').contains(&ch) {
    return false;
  }
  let unicode = matches!(ch, '\u{85}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10fffe}');
  !unicode || ch == '\u{feff}'
}

/// The tag that PyYAML's resolver gives a plain scalar, or `None` when the scalar reads back as a string.
fn implicit_tag(value: &str) -> Option<&'static str> {
  // PyYAML tries only the resolvers registered for the first character. An empty scalar is tried against all of
  // them, and only the null pattern matches the empty string.
  implicit_resolvers()
    .iter()
    .find(|resolver| {
      value.chars().next().is_none_or(|first| resolver.first.contains(first)) && resolver.pattern.is_match(value)
    })
    .map(|resolver| resolver.tag)
}

/// One of PyYAML's implicit resolvers (`yaml/resolver.py` in PyYAML 6.0): a plain scalar that matches `pattern` reads
/// back as `tag`. `first` lists the first characters that PyYAML tries the pattern on.
struct ImplicitResolver {
  tag: &'static str,
  pattern: Regex,
  first: &'static str,
}

/// PyYAML's implicit resolvers in registration order. The `yaml` resolver is left out: its pattern matches `!`, `&` and
/// `*`, which a plain scalar cannot start with.
fn implicit_resolvers() -> &'static [ImplicitResolver] {
  static RESOLVERS: OnceLock<Vec<ImplicitResolver>> = OnceLock::new();
  RESOLVERS
    .get_or_init(|| {
      let resolver = |tag: &'static str, pattern: &str, first: &'static str| ImplicitResolver {
        tag,
        pattern: Regex::new(pattern).expect("PyYAML's resolver patterns are valid regexes"),
        first,
      };
      vec![
        resolver(
          "tag:yaml.org,2002:bool",
          r"^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF)$",
          "yYnNtTfFoO",
        ),
        resolver(
          "tag:yaml.org,2002:float",
          concat!(
            r"^(?:[-+]?[0-9][0-9_]*\.[0-9_]*(?:[eE][-+][0-9]+)?",
            r"|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?",
            r"|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*",
            r"|[-+]?\.(?:inf|Inf|INF)",
            r"|\.(?:nan|NaN|NAN))$",
          ),
          "-+0123456789.",
        ),
        resolver(
          "tag:yaml.org,2002:int",
          concat!(
            r"^(?:[-+]?0b[0-1_]+",
            r"|[-+]?0[0-7_]+",
            r"|[-+]?(?:0|[1-9][0-9_]*)",
            r"|[-+]?0x[0-9a-fA-F_]+",
            r"|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$",
          ),
          "-+0123456789",
        ),
        resolver("tag:yaml.org,2002:merge", r"^(?:<<)$", "<"),
        resolver("tag:yaml.org,2002:null", r"^(?:~|null|Null|NULL|)$", "~nN"),
        resolver(
          "tag:yaml.org,2002:timestamp",
          concat!(
            r"^(?:[0-9]{4}-[0-9]{2}-[0-9]{2}",
            r"|[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?",
            r"(?:[ \t]*(?:Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?)$",
          ),
          "0123456789",
        ),
        resolver("tag:yaml.org,2002:value", r"^(?:=)$", "="),
      ]
    })
    .as_slice()
}

/// The output line that a scalar is written onto. `column` counts characters from the line start, as PyYAML's emitter
/// does, and folded lines continue at `indent`.
struct Line<'a> {
  output: &'a mut String,
  column: usize,
  indent: usize,
}

impl<'a> Line<'a> {
  fn new(output: &'a mut String, indent: usize) -> Self {
    let column = current_column(output);
    Self { output, column, indent }
  }

  fn push(&mut self, text: &[char]) {
    self.output.extend(text);
    self.column += text.len();
  }

  fn push_str(&mut self, text: &str) {
    self.output.push_str(text);
    self.column += text.chars().count();
  }

  fn line_break(&mut self, br: char) {
    self.output.push(br);
    self.column = 0;
  }

  /// Pads the new line to the indent. Every caller has just written a line break, so the column is 0.
  fn write_indent(&mut self) {
    self.output.push_str(&" ".repeat(self.indent));
    self.column = self.indent;
  }

  fn fold(&mut self) {
    self.line_break('\n');
    self.write_indent();
  }
}

/// Pads the output's current line to `indent`, so that a mapping entry or a sequence item starts at that column. The
/// line is empty, or it ends with a `- ` marker that already reaches `indent`.
fn pad_to(output: &mut String, indent: usize) {
  let pad = indent.checked_sub(current_column(output)).expect("an entry never starts past its indent");
  output.push_str(&" ".repeat(pad));
}

/// The number of characters after the output's last line break.
fn current_column(output: &str) -> usize {
  output.rsplit('\n').next().expect("rsplit yields at least one piece").chars().count()
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::dump;

  #[test]
  fn dump_keeps_cron_yaml_mapping_order_and_pyyaml_string_quoting() {
    assert_eq!(
      dump(&json!({"name": "daily", "cron": "0 8 * * *", "enabled": true, "prompt": "line one\n\nline two"})).unwrap(),
      "name: daily\ncron: 0 8 * * *\nenabled: true\nprompt: 'line one\n\n\n  line two'\n"
    );
    assert_eq!(dump(&json!({"value": "on"})).unwrap(), "value: 'on'\n");
    assert_eq!(dump(&json!({"value": "雪と café"})).unwrap(), "value: 雪と café\n");
  }

  #[test]
  fn malformed_yaml_remains_an_error() {
    // Python: tests/test_yaml_utils.py::test_malformed_document_raises_yaml_error
    assert!(serde_yaml::from_str::<serde_json::Value>("a: [1, 2\nb: {c").is_err());
  }
}
