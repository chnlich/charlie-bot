//! One server log line: `<time> [<level>] <location> <event> <key>=<value>...`, with the time in UTC to the
//! millisecond and the location the file and line of the tracing call. `render` writes a line and `parse` reads one
//! back. `parse` also reads the two forms the server wrote before this one: the text form without a location column,
//! which a release before this grammar writes after a rollback, and the JSON lines the Python server wrote.
//!
//! ```text
//! line     := stamp SP "[" level "]" SP location SP event [SP field]*
//! stamp    := YYYY-MM-DD SP HH:MM:SS "." mmm "Z"
//! level    := error | warning | info | debug | trace, padded to 9 columns inside the brackets
//! location := bare "<path>:<line>", padded to 30 columns
//! event    := value, padded to 30 columns
//! field    := key "=" value, in call order
//! value    := bare | quoted
//! bare     := non-empty, with no whitespace, control character, "=", "'" or '"'
//! quoted   := a quote wrapping the output of `text_repr::str_repr`; `parse` decodes it as str_repr's inverse
//! ```
//!
//! Padding is for reading by eye: `parse` splits outside quotes, so padding never reaches a value. `parse` tells the
//! location column from an old-form line's event by the rule `is_location_column` pins: the location is always written
//! bare and ends in `:<line>`, and an event always follows it.

use std::str::CharIndices;

use serde_json::{Map, Value};
use tracing::Level;

use crate::text_repr;

/// The width of the level word inside its brackets, of the location column, and of the event column.
const LEVEL_WIDTH: usize = 9;
const LOCATION_WIDTH: usize = 30;
const EVENT_WIDTH: usize = 30;

/// Each tracing level with the word its lines carry.
const LEVELS: [(Level, &str); 5] = [
  (Level::ERROR, "error"),
  (Level::WARN, "warning"),
  (Level::INFO, "info"),
  (Level::DEBUG, "debug"),
  (Level::TRACE, "trace"),
];

/// The stamp's shape: `d` is a digit, and every other byte must match itself.
const STAMP_SHAPE: &[u8] = b"dddd-dd-dd dd:dd:dd.dddZ";

/// A parsed line: its location column, its event name, and its fields in file order. A value is a string; a number or
/// a boolean keeps its JSON spelling, so `200` and `true` read the same from either form. A line of an older form has
/// no location column, and its row holds the empty string there.
#[derive(Debug, PartialEq, Eq)]
pub struct Row {
  pub location: String,
  pub event: String,
  pub fields: Vec<(String, String)>,
}

impl Row {
  /// The value of the first field named `key`.
  pub fn field(&self, key: &str) -> Option<&str> {
    self.fields.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
  }
}

/// The level word a tracing level writes.
pub fn level_word(level: &Level) -> &'static str {
  LEVELS.iter().find(|(known, _)| known == level).map(|(_, word)| *word).expect("every tracing level has a word")
}

/// One line. `time` is a `timefmt::log_timestamp` stamp, `level` a level word, and `location` a `logging` location
/// text, a bare `<path>:<line>`. A field value is a string, or a JSON value written in its JSON spelling.
pub fn render(time: &str, level: &str, location: &str, event: &str, fields: &[(String, Value)]) -> String {
  let mut line = format!("{time} [{level:<LEVEL_WIDTH$}] {location:<LOCATION_WIDTH$} {:<EVENT_WIDTH$}", written(event));
  for (key, value) in fields {
    line.push(' ');
    line.push_str(key);
    line.push('=');
    line.push_str(&written(&value_text(value)));
  }
  line.truncate(line.trim_end().len());
  line
}

/// Whether a value must be quoted: it is empty, or it holds whitespace, a control character, `=`, `'` or `"`.
pub fn needs_quotes(text: &str) -> bool {
  text.is_empty() || text.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '=' | '\'' | '"'))
}

/// Reads a line in either form. A JSON line is an object with an `event` key, and a text line follows the grammar.
/// Any other line gives `None`, and a reader skips it.
pub fn parse(line: &str) -> Option<Row> {
  let line = line.trim();
  if line.starts_with('{') { parse_json(line) } else { parse_text(line) }
}

/// The text of a value as a line holds it: quoted when `needs_quotes` says so.
fn written(text: &str) -> String {
  if needs_quotes(text) { text_repr::str_repr(text) } else { text.to_string() }
}

fn is_bare(text: &str) -> bool {
  !needs_quotes(text)
}

/// A JSON value's text: a string as it is, any other value in its JSON spelling.
pub(crate) fn value_text(value: &Value) -> String {
  match value {
    Value::String(text) => text.clone(),
    other => other.to_string(),
  }
}

fn parse_json(line: &str) -> Option<Row> {
  let object: Map<String, Value> = serde_json::from_str(line).ok()?;
  let event = object.get("event")?.as_str()?.to_string();
  let fields = object
    .iter()
    .filter(|(key, _)| !matches!(key.as_str(), "event" | "level" | "timestamp"))
    .map(|(key, value)| (key.clone(), value_text(value)))
    .collect();
  Some(Row { location: String::new(), event, fields })
}

fn parse_text(line: &str) -> Option<Row> {
  let rest = strip_level(strip_stamp(line)?)?;
  let first = rest.trim_start();
  // The location column is always written bare, so a quoted first value is an old-form line's event.
  let bare = !first.starts_with(['\'', '"']);
  let (first_value, rest) = read_value(first)?;
  let after_first = rest.trim_start();
  let (location, event, rest) = if is_location_column(bare, &first_value, after_first) {
    let (event, rest) = read_value(after_first)?;
    (first_value, event, rest)
  } else {
    (String::new(), first_value, rest)
  };
  let mut fields = Vec::new();
  let mut rest = rest.trim_start();
  while !rest.is_empty() {
    let (key, after_key) = rest.split_once('=')?;
    if !is_bare(key) {
      return None;
    }
    let (value, after_value) = read_value(after_key)?;
    fields.push((key.to_string(), value));
    rest = after_value.trim_start();
  }
  Some(Row { location, event, fields })
}

/// Whether the first value after the level is the location column. It is: written bare, ending in `:<line>`, and
/// followed by the event. An old-form line has no location, and its event comes first instead.
fn is_location_column(bare: bool, value: &str, after: &str) -> bool {
  bare
    && !after.is_empty()
    && value
      .rsplit_once(':')
      .is_some_and(|(_, line)| !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The text after a stamp and the space that follows it.
fn strip_stamp(line: &str) -> Option<&str> {
  let stamp = line.as_bytes().get(..STAMP_SHAPE.len())?;
  let shaped = stamp.iter().zip(STAMP_SHAPE).all(|(byte, shape)| match *shape {
    b'd' => byte.is_ascii_digit(),
    _ => byte == shape,
  });
  if !shaped {
    return None;
  }
  line.get(STAMP_SHAPE.len()..)?.strip_prefix(' ')
}

/// The text after a bracketed level word, its closing bracket and the space that follows it.
fn strip_level(text: &str) -> Option<&str> {
  let (inside, after) = text.strip_prefix('[')?.split_once(']')?;
  let word = inside.trim_end();
  if inside.len() != LEVEL_WIDTH || !LEVELS.iter().any(|(_, known)| *known == word) {
    return None;
  }
  after.strip_prefix(' ')
}

/// The value at the start of `text` and the text after it. A bare value ends at whitespace or the end of the line.
fn read_value(text: &str) -> Option<(String, &str)> {
  let (value, rest) = if text.starts_with(['\'', '"']) {
    unquote(text)?
  } else {
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    if !is_bare(&text[..end]) {
      return None;
    }
    (text[..end].to_string(), &text[end..])
  };
  (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some((value, rest))
}

/// The value of the quoted text at the start of `text`, decoded as the inverse of `text_repr::str_repr`, and the text after
/// its closing quote.
fn unquote(text: &str) -> Option<(String, &str)> {
  let mut chars = text.char_indices();
  let (_, quote) = chars.next()?;
  let mut value = String::new();
  while let Some((index, c)) = chars.next() {
    match c {
      _ if c == quote => return Some((value, &text[index + 1..])),
      '\\' => {
        let (_, escape) = chars.next()?;
        value.push(match escape {
          '\\' | '\'' | '"' => escape,
          't' => '\t',
          'n' => '\n',
          'r' => '\r',
          'x' => hex_char(&mut chars, 2)?,
          'u' => hex_char(&mut chars, 4)?,
          'U' => hex_char(&mut chars, 8)?,
          _ => return None,
        });
      }
      _ => value.push(c),
    }
  }
  None
}

/// The character that a `\x`, `\u` or `\U` escape names, from its `digits` hex digits.
fn hex_char(chars: &mut CharIndices<'_>, digits: usize) -> Option<char> {
  let mut code = 0;
  for _ in 0..digits {
    let (_, digit) = chars.next()?;
    code = code * 16 + digit.to_digit(16)?;
  }
  char::from_u32(code)
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  const TIME: &str = "2026-10-29 07:11:32.244Z";

  /// Characters that the quoting rule and the escapes treat differently: ASCII and non-ASCII whitespace, control
  /// characters, both quotes, the backslash, and letters beside them.
  const POOL: [char; 20] = [
    'a',
    'x',
    ' ',
    '=',
    '\'',
    '"',
    '\\',
    '\n',
    '\r',
    '\t',
    '\u{1}',
    '\u{7f}',
    '\u{85}',
    '\u{a0}',
    '\u{2028}',
    '\u{3000}',
    '\u{200b}',
    'é',
    '中',
    '\u{1f600}',
  ];

  /// Every string of up to three characters from `POOL`, starting with the empty string.
  fn generated() -> Vec<String> {
    let mut all = vec![String::new()];
    let mut frontier = vec![String::new()];
    for _ in 0..3 {
      frontier = frontier.iter().flat_map(|prefix| POOL.into_iter().map(move |c| format!("{prefix}{c}"))).collect();
      all.extend(frontier.iter().cloned());
    }
    all
  }

  #[test]
  fn a_line_names_its_time_level_location_and_event_then_its_fields_in_call_order() {
    let fields = [
      ("method".to_string(), json!("GET")),
      ("path".to_string(), json!("/api/internal/version")),
      ("status".to_string(), json!(200)),
      ("duration_ms".to_string(), json!(5)),
      ("client".to_string(), json!("127.0.0.1")),
    ];
    assert_eq!(
      render(TIME, "info", "server/request_log.rs:39", "http_request", &fields),
      concat!(
        "2026-10-29 07:11:32.244Z [info     ] server/request_log.rs:39       http_request                   ",
        "method=GET path=/api/internal/version status=200 duration_ms=5 client=127.0.0.1"
      )
    );
  }

  #[test]
  fn a_line_reads_back_with_its_location_and_a_longer_location_pushes_only_its_own_line() {
    let line = render(TIME, "info", "server/request_log.rs:39", "http_request", &[]);
    assert_eq!(
      parse(&line).expect("the line is read"),
      Row { location: "server/request_log.rs:39".to_string(), event: "http_request".to_string(), fields: vec![] }
    );
    let long = render(TIME, "info", "serde_json-1.0.145/src/de/serde/json.rs:1234", "http_request", &[]);
    assert!(long.contains(" http_request"), "{long}");
    assert_eq!(parse(&long).expect("the line is read").location, "serde_json-1.0.145/src/de/serde/json.rs:1234");
  }

  #[test]
  fn an_old_form_line_without_a_location_column_reads_to_an_empty_location() {
    let old = "2026-10-29 07:11:32.244Z [info     ] http_request                   method=GET status=200";
    let row = parse(old).expect("the line is read");
    assert_eq!(row.location, "");
    assert_eq!(row.event, "http_request");
    assert_eq!(row.fields, [("method".to_string(), "GET".to_string()), ("status".to_string(), "200".to_string())]);
  }

  #[test]
  fn the_location_column_is_bare_and_ends_in_a_line_number_before_the_event() {
    // The current form: the location is read first, and an event that itself ends in `:<digits>` reads as the event.
    let both = format!("{} [info     ] core/logging.rs:12          a:1", TIME);
    let row = parse(&both).expect("the line is read");
    assert_eq!(row.location, "core/logging.rs:12");
    assert_eq!(row.event, "a:1");
    // The old form: a last column with no value after it is the event, whatever it ends in.
    let old = format!("{} [info     ] server_ready:12", TIME);
    let row = parse(&old).expect("the line is read");
    assert_eq!(row.location, "");
    assert_eq!(row.event, "server_ready:12");
  }

  #[test]
  fn an_event_with_spaces_is_quoted_and_reads_back_as_one_event() {
    let fields = [("error".to_string(), json!("TypeError"))];
    let line = render(
      "2026-10-29 07:12:05.118Z",
      "error",
      "slack/slack_listener.rs:245",
      "Exception in ASGI application",
      &fields,
    );
    assert_eq!(
      line,
      "2026-10-29 07:12:05.118Z [error    ] slack/slack_listener.rs:245    'Exception in ASGI application' error=TypeError"
    );
    let row = parse(&line).expect("the line is read");
    assert_eq!(row.event, "Exception in ASGI application");
    assert_eq!(row.field("error"), Some("TypeError"));
    let old = "2026-10-29 07:12:05.118Z [error    ] 'Exception in ASGI application' error=TypeError";
    let row = parse(old).expect("the old line is read");
    assert_eq!(row.location, "");
    assert_eq!(row.event, "Exception in ASGI application");
    assert_eq!(row.field("error"), Some("TypeError"));
  }

  #[test]
  fn every_generated_value_and_event_reads_back_unchanged_on_one_line() {
    for text in generated() {
      let fields = [("key".to_string(), Value::String(text.clone()))];
      let line = render(TIME, "info", "core/logging.rs:12", &text, &fields);
      assert!(!line.contains(['\n', '\r']), "{line:?}");
      let row = Row {
        location: "core/logging.rs:12".to_string(),
        event: text.clone(),
        fields: vec![("key".to_string(), text.clone())],
      };
      assert_eq!(parse(&line), Some(row), "{line:?}");
      let bare_event = render(TIME, "info", "core/logging.rs:12", &text, &[]);
      let row = Row { location: "core/logging.rs:12".to_string(), event: text, fields: vec![] };
      assert_eq!(parse(&bare_event), Some(row), "{bare_event:?}");
    }
  }

  #[test]
  fn a_json_line_and_its_text_line_read_to_the_same_row() {
    let json = concat!(
      r#"{"event":"http_request","method":"GET","flag":true,"status":200,"nothing":null,"#,
      r#""level":"info","timestamp":"2026-10-29T07:11:32.244911Z"}"#
    );
    let text = concat!(
      "2026-10-29 07:11:32.244Z [info     ] http_request                   ",
      "method=GET flag=true status=200 nothing=null"
    );
    let expected = || Row {
      location: String::new(),
      event: "http_request".to_string(),
      fields: vec![
        ("method".to_string(), "GET".to_string()),
        ("flag".to_string(), "true".to_string()),
        ("status".to_string(), "200".to_string()),
        ("nothing".to_string(), "null".to_string()),
      ],
    };
    assert_eq!(parse(json), Some(expected()));
    assert_eq!(parse(text), Some(expected()));
  }

  #[test]
  fn a_line_outside_the_grammar_reads_as_none() {
    for line in [
      "2026-10-26 02:16:37 [info     ] http_request                   client=127.0.0.1",
      "2026-10-29 07:11:32.244Z [loud     ] server_ready",
      "2026-10-29 07:11:32.244Z [info     ] server_ready ready_in_ms",
      "2026-10-29 07:11:32.244Z [info     ] server_ready a=b=c",
      "2026-10-29 07:11:32.244Z [info     ] server_ready a='\\q'",
      "Starting charliebot-server",
      "{\"level\": \"info\"}",
      "",
    ] {
      assert_eq!(parse(line), None, "{line}");
    }
  }
}
