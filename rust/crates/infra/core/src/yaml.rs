//! The `yaml_utils.save_yaml` writer (`src/infra/yaml_utils.py`).
//!
//! Scalar style follows PyYAML 6's resolver and block-context scalar analysis. Quoted scalars use its escaping and
//! width-80 folding rules, and floating-point numbers use its `represent_float` spelling.

use std::sync::OnceLock;

use crate::text_repr;
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
    Value::Number(value) if value.is_f64() => {
      write_float(output, value.as_f64().expect("a serde_json floating-point number has an f64 value"));
    }
    Value::Number(value) => output.push_str(&value.to_string()),
    Value::String(value) => write_string(output, value, false, indent),
    Value::Array(_) | Value::Object(_) => unreachable!("write_item and write_pair write containers"),
  }
}

/// Writes a floating-point value using PyYAML's `represent_float` format.
fn write_float(output: &mut String, value: f64) {
  if value.is_nan() {
    output.push_str(".nan");
  } else if value == f64::INFINITY {
    output.push_str(".inf");
  } else if value == f64::NEG_INFINITY {
    output.push_str("-.inf");
  } else {
    let mut repr = text_repr::float_repr(value).to_lowercase();
    if !repr.contains('.')
      && let Some(exponent) = repr.find('e')
    {
      repr.insert_str(exponent, ".0");
    }
    output.push_str(&repr);
  }
}

/// Writes a string in the style that PyYAML's emitter picks: plain when the text reads back as a string, single-quoted
/// when it allows it. Otherwise it is double-quoted, with PyYAML escapes and folding.
fn write_string(output: &mut String, value: &str, key: bool, indent: usize) {
  let analysis = analyze_scalar(value);
  if analysis.allow_block_plain && implicit_tag(value).is_none() {
    write_plain(output, value, indent, !key);
  } else if analysis.allow_single_quoted && !(key && analysis.multiline) {
    write_single_quoted(output, value, indent, !key);
  } else {
    write_double_quoted(output, value, indent, !key);
  }
}

/// Writes a double-quoted scalar using PyYAML's escape table and column-80 folding rules.
fn write_double_quoted(output: &mut String, text: &str, indent: usize, split: bool) {
  let chars: Vec<char> = text.chars().collect();
  let mut line = Line::new(output, indent);
  line.push_str("\"");
  let mut start = 0;
  let mut end = 0;
  while end <= chars.len() {
    let ch = chars.get(end).copied();
    if ch.is_none_or(requires_double_quoted_escape) {
      if start < end {
        line.push(&chars[start..end]);
        start = end;
      }
      if let Some(ch) = ch {
        line.push_str(&double_quoted_escape(ch));
        start = end + 1;
      }
    }
    let column_at_end = line.column as isize + end as isize - start as isize;
    if split
      && ch == Some(' ')
      && end > 0
      && chars[end - 1] != ' '
      && end < chars.len() - 1
      && column_at_end > BEST_WIDTH as isize
    {
      if start < end {
        line.push(&chars[start..end]);
      }
      line.fold();
      if chars[end + 1] == ' ' {
        line.push_str("\\");
      }
      start = end + 1;
    }
    end += 1;
  }
  line.push_str("\"");
}

/// Whether PyYAML's double-quoted writer escapes the character with its replacement or a hexadecimal escape.
fn requires_double_quoted_escape(ch: char) -> bool {
  matches!(ch, '"' | '\\' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}')
    || !((' '..='~').contains(&ch) || ('\u{a0}'..='\u{d7ff}').contains(&ch) || ('\u{e000}'..='\u{fffd}').contains(&ch))
}

/// Returns the escape PyYAML writes for a character that cannot appear directly in a double-quoted scalar.
fn double_quoted_escape(ch: char) -> String {
  match ch {
    '\0' => "\\0".to_string(),
    '\u{7}' => "\\a".to_string(),
    '\u{8}' => "\\b".to_string(),
    '\t' => "\\t".to_string(),
    '\n' => "\\n".to_string(),
    '\u{b}' => "\\v".to_string(),
    '\u{c}' => "\\f".to_string(),
    '\r' => "\\r".to_string(),
    '\u{1b}' => "\\e".to_string(),
    '"' => "\\\"".to_string(),
    '\\' => "\\\\".to_string(),
    '\u{85}' => "\\N".to_string(),
    '\u{a0}' => "\\_".to_string(),
    '\u{2028}' => "\\L".to_string(),
    '\u{2029}' => "\\P".to_string(),
    _ if ch as u32 <= 0xff => format!("\\x{:02X}", ch as u32),
    _ if ch as u32 <= 0xffff => format!("\\u{:04X}", ch as u32),
    _ => format!("\\U{:08X}", ch as u32),
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

struct ScalarAnalysis {
  allow_block_plain: bool,
  allow_single_quoted: bool,
  multiline: bool,
}

/// Returns the scalar style flags used by PyYAML's block-context emitter.
fn analyze_scalar(value: &str) -> ScalarAnalysis {
  let chars: Vec<char> = value.chars().collect();
  let is_yaml_whitespace = |ch| matches!(ch, '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}');
  let mut block_indicators = value.starts_with("---") || value.starts_with("...");
  let mut line_breaks = false;
  let mut special_characters = false;
  let mut leading_space = false;
  let mut leading_break = false;
  let mut trailing_space = false;
  let mut trailing_break = false;
  let mut break_space = false;
  let mut space_break = false;
  let mut preceded_by_whitespace = true;
  let mut previous_space = false;
  let mut previous_break = false;

  for (index, &ch) in chars.iter().enumerate() {
    let followed_by_whitespace = chars.get(index + 1).is_none_or(|&next| is_yaml_whitespace(next));
    if index == 0 {
      if matches!(ch, '#' | ',' | '[' | ']' | '{' | '}' | '&' | '*' | '!' | '|' | '>' | '\'' | '"' | '%' | '@' | '`') {
        block_indicators = true;
      }
      if matches!(ch, '?' | ':') && followed_by_whitespace {
        block_indicators = true;
      }
      if ch == '-' && followed_by_whitespace {
        block_indicators = true;
      }
    } else {
      if ch == ':' && followed_by_whitespace {
        block_indicators = true;
      }
      if ch == '#' && preceded_by_whitespace {
        block_indicators = true;
      }
    }

    if is_break(ch) {
      line_breaks = true;
    }
    if ch != '\n' && !(' '..='~').contains(&ch) {
      let allowed_unicode =
        matches!(ch, '\u{85}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}') && ch != '\u{feff}';
      if !allowed_unicode || ch == '\u{85}' {
        special_characters = true;
      }
    }

    if ch == ' ' {
      leading_space |= index == 0;
      trailing_space |= index == chars.len() - 1;
      break_space |= previous_break;
      previous_space = true;
      previous_break = false;
    } else if is_break(ch) {
      leading_break |= index == 0;
      trailing_break |= index == chars.len() - 1;
      space_break |= previous_space;
      previous_space = false;
      previous_break = true;
    } else {
      previous_space = false;
      previous_break = false;
    }
    preceded_by_whitespace = is_yaml_whitespace(ch);
  }

  let allow_block_plain = !leading_space
    && !leading_break
    && !trailing_space
    && !trailing_break
    && !break_space
    && !space_break
    && !special_characters
    && !line_breaks
    && !block_indicators;
  let allow_single_quoted = !break_space && !space_break && !special_characters;
  ScalarAnalysis { allow_block_plain, allow_single_quoted, multiline: line_breaks }
}

/// The characters that PyYAML's emitter treats as line breaks.
fn is_break(ch: char) -> bool {
  matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
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

  use super::{dump, write_float};

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

  #[test]
  fn dump_formats_json_floats_and_non_finite_float_values_like_pyyaml() {
    let finite = serde_json::Value::Number(serde_json::Number::from_f64(0.5).expect("0.5 is a finite JSON number"));
    assert_eq!(dump(&finite).unwrap(), "0.5\n");

    for (value, expected) in [(f64::INFINITY, ".inf"), (f64::NEG_INFINITY, "-.inf"), (f64::NAN, ".nan")] {
      assert!(serde_json::Number::from_f64(value).is_none());
      let mut output = String::new();
      write_float(&mut output, value);
      assert_eq!(output, expected);
    }
  }
}
