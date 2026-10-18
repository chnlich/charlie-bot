//! Text renderings that reproduce CPython's: `repr(float)`, `repr(str)`, `str(...)` of a JSON value, and
//! `json.dumps` under the option sets the Python server uses.
//!
//! The Python server's wire bytes come from these renderings (the Jinja `tojson` filter, the starlette
//! JSON responses, the fence identity record, the static-file ETag), so each one follows the CPython
//! 3.14 algorithm it names rather than serde_json's own formatting.

use serde_json::Value;

/// One `json.dumps` option set.
#[derive(Debug, Clone, Copy)]
pub struct JsonStyle {
  /// `ensure_ascii`: escape every character outside `' '..='~'` as `\uXXXX`.
  pub ensure_ascii: bool,
  /// `sort_keys`: emit object members in key order instead of insertion order.
  pub sort_keys: bool,
  /// The item separator (`separators[0]`).
  pub item_separator: &'static str,
  /// The key separator (`separators[1]`).
  pub key_separator: &'static str,
  /// `indent`: spaces per nesting level, or `None` for a single line.
  pub indent: Option<usize>,
}

/// Jinja2's `tojson` filter: `json.dumps(value, sort_keys=True)` with the default separators.
pub const JINJA_TOJSON: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: true, item_separator: ", ", key_separator: ": ", indent: None };

/// starlette's `JSONResponse.render`: `ensure_ascii=False`, `separators=(",", ":")`, insertion order.
pub const STARLETTE_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ":", indent: None };

/// `json.dumps(value)` with every option at its default.
pub const PYTHON_JSON_DEFAULT: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// `json.dumps(value, indent=2, sort_keys=True)`: the home writer fence identity record.
pub const FENCE_RECORD_JSON: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: true, item_separator: ",", key_separator: ": ", indent: Some(2) };

/// `json.dumps(value, separators=(",", ":"), sort_keys=True)`: the run-token payload.
pub const RUN_TOKEN_PAYLOAD_JSON: JsonStyle =
  JsonStyle { ensure_ascii: true, sort_keys: true, item_separator: ",", key_separator: ":", indent: None };

/// CPython `repr(float)`: the shortest round-trip digits, exponent form when the decimal point
/// position is below -3 or above 16, and a trailing `.0` on integral values.
pub fn float_repr(x: f64) -> String {
  if x.is_nan() {
    return "nan".to_string();
  }
  if x.is_infinite() {
    return if x > 0.0 { "inf" } else { "-inf" }.to_string();
  }
  if x == 0.0 {
    return if x.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
  }
  // Rust's LowerExp without a precision prints the shortest round-trip mantissa, the same digit
  // string CPython's repr picks; only the layout differs.
  let scientific = format!("{x:e}");
  let (mantissa, exponent) = scientific.split_once('e').expect("LowerExp output carries an exponent");
  let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
  let negative = mantissa.starts_with('-');
  let digits: String = mantissa.trim_start_matches('-').chars().filter(|c| *c != '.').collect();
  let point = exponent + 1;
  let body = if point <= -4 || point > 16 {
    let mut text = digits[..1].to_string();
    if digits.len() > 1 {
      text.push('.');
      text.push_str(&digits[1..]);
    }
    text.push('e');
    text.push(if exponent < 0 { '-' } else { '+' });
    text.push_str(&format!("{:02}", exponent.abs()));
    text
  } else if point <= 0 {
    format!("0.{}{}", "0".repeat(point.unsigned_abs() as usize), digits)
  } else if point as usize >= digits.len() {
    format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
  } else {
    format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
  };
  if negative { format!("-{body}") } else { body }
}

/// CPython `repr(str)` for the strings this server reports (purposes, paths, config keys).
///
/// The quote is `'` unless the text holds `'` and no `"`. Backslash, the quote, `\t`, `\n`, `\r` and the
/// other C0 controls and DEL are escaped; every other character prints as itself. CPython also escapes
/// non-printable non-ASCII characters (`str.isprintable`), which this rendering leaves as they are.
pub fn str_repr(text: &str) -> String {
  let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
  let mut out = String::with_capacity(text.len() + 2);
  out.push(quote);
  for c in text.chars() {
    match c {
      '\\' => out.push_str("\\\\"),
      '\t' => out.push_str("\\t"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      c if c == quote => {
        out.push('\\');
        out.push(c);
      }
      c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
      c => out.push(c),
    }
  }
  out.push(quote);
  out
}

/// CPython `str(value)` of a value `json.loads` produced: strings print bare, the rest as their repr.
pub fn python_str(value: &Value) -> String {
  match value {
    Value::String(text) => text.clone(),
    other => python_repr(other),
  }
}

fn python_repr(value: &Value) -> String {
  match value {
    Value::Null => "None".to_string(),
    Value::Bool(true) => "True".to_string(),
    Value::Bool(false) => "False".to_string(),
    Value::Number(number) => match number.as_f64() {
      Some(float) if number.is_f64() => float_repr(float),
      _ => number.to_string(),
    },
    Value::String(text) => str_repr(text),
    Value::Array(items) => format!("[{}]", items.iter().map(python_repr).collect::<Vec<_>>().join(", ")),
    Value::Object(members) => format!(
      "{{{}}}",
      members
        .iter()
        .map(|(key, item)| format!("{}: {}", str_repr(key), python_repr(item)))
        .collect::<Vec<_>>()
        .join(", ")
    ),
  }
}

/// `json.dumps(value, ...)` under `style`. NaN and the infinities print as CPython's `allow_nan` forms.
pub fn json_dumps(value: &Value, style: &JsonStyle) -> String {
  let mut out = String::new();
  write_json(&mut out, value, style, 0);
  out
}

fn write_json(out: &mut String, value: &Value, style: &JsonStyle, level: usize) {
  match value {
    Value::Null => out.push_str("null"),
    Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
    Value::Number(number) => {
      if number.is_f64() {
        let float = number.as_f64().expect("an f64 number reads as f64");
        out.push_str(&if float.is_nan() {
          "NaN".to_string()
        } else if float.is_infinite() {
          if float > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
        } else {
          float_repr(float)
        });
      } else {
        out.push_str(&number.to_string());
      }
    }
    Value::String(text) => write_json_str(out, text, style.ensure_ascii),
    Value::Array(items) => {
      if items.is_empty() {
        out.push_str("[]");
        return;
      }
      out.push('[');
      for (index, item) in items.iter().enumerate() {
        write_separator(out, style, level + 1, index == 0);
        write_json(out, item, style, level + 1);
      }
      write_closing_indent(out, style, level);
      out.push(']');
    }
    Value::Object(members) => {
      if members.is_empty() {
        out.push_str("{}");
        return;
      }
      let mut entries: Vec<(&String, &Value)> = members.iter().collect();
      if style.sort_keys {
        entries.sort_by(|left, right| left.0.cmp(right.0));
      }
      out.push('{');
      for (index, (key, item)) in entries.into_iter().enumerate() {
        write_separator(out, style, level + 1, index == 0);
        write_json_str(out, key, style.ensure_ascii);
        out.push_str(style.key_separator);
        write_json(out, item, style, level + 1);
      }
      write_closing_indent(out, style, level);
      out.push('}');
    }
  }
}

fn write_separator(out: &mut String, style: &JsonStyle, level: usize, first: bool) {
  if !first {
    out.push_str(style.item_separator);
  }
  if let Some(indent) = style.indent {
    out.push('\n');
    out.push_str(&" ".repeat(indent * level));
  }
}

fn write_closing_indent(out: &mut String, style: &JsonStyle, level: usize) {
  if let Some(indent) = style.indent {
    out.push('\n');
    out.push_str(&" ".repeat(indent * level));
  }
}

/// Appends one JSON string literal as `json.dumps` writes it: the same escapes for every renderer, with
/// characters above `~` as `\uXXXX` pairs when `ensure_ascii`.
pub fn write_json_str(out: &mut String, text: &str, ensure_ascii: bool) {
  out.push('"');
  for c in text.chars() {
    match c {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      '\u{8}' => out.push_str("\\b"),
      '\u{c}' => out.push_str("\\f"),
      c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
      c if ensure_ascii && (c as u32) > 0x7e => {
        let mut units = [0u16; 2];
        for unit in c.encode_utf16(&mut units) {
          out.push_str(&format!("\\u{unit:04x}"));
        }
      }
      c => out.push(c),
    }
  }
  out.push('"');
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn float_repr_matches_cpython() {
    let cases = [
      (1760771740.0, "1760771740.0"),
      (1760771740.1234567, "1760771740.1234567"),
      (0.1, "0.1"),
      (1e16, "1e+16"),
      (1234567890123456.0, "1234567890123456.0"),
      (0.0001, "0.0001"),
      (0.00001, "1e-05"),
      (-2.5, "-2.5"),
      (1.5e-7, "1.5e-07"),
      (0.0, "0.0"),
    ];
    for (value, expected) in cases {
      assert_eq!(float_repr(value), expected, "repr({value})");
    }
  }

  #[test]
  fn tojson_style_sorts_keys_and_escapes_non_ascii() {
    let value = json!({"codex": {"manual": false, "hint": "only — auto"}, "cc-claude": {"manual": true, "hint": null}});
    assert_eq!(
      json_dumps(&value, &JINJA_TOJSON),
      r#"{"cc-claude": {"hint": null, "manual": true}, "codex": {"hint": "only \u2014 auto", "manual": false}}"#
    );
    assert_eq!(json_dumps(&json!("CC · Fable\u{7f}"), &JINJA_TOJSON), r#""CC \u00b7 Fable\u007f""#);
  }

  #[test]
  fn indented_style_matches_json_dumps_indent_2() {
    let value = json!({"pid": 7, "argv": "a b", "pid_start": null});
    assert_eq!(
      json_dumps(&value, &FENCE_RECORD_JSON),
      "{\n  \"argv\": \"a b\",\n  \"pid\": 7,\n  \"pid_start\": null\n}"
    );
  }

  #[test]
  fn str_repr_picks_the_quote_like_cpython() {
    assert_eq!(str_repr("server startup"), "'server startup'");
    assert_eq!(str_repr("it's"), "\"it's\"");
    assert_eq!(str_repr("a'b\"c"), "'a\\'b\"c'");
  }
}
