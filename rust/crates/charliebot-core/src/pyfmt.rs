//! Text renderings that reproduce CPython's: `repr(float)`, `repr(str)`, `str(...)` of a JSON value, and
//! `json.dumps` under the option sets the Python server uses.
//!
//! The Python server's wire bytes come from these renderings (the Jinja `tojson` filter, the starlette
//! JSON responses, the fence identity record, the static-file ETag), so each one follows the CPython
//! 3.14 algorithm it names rather than serde_json's own formatting.

use crate::printable_table::NON_PRINTABLE;
use serde::Serialize;
use serde_json::ser::Formatter as JsonFormatter;
use serde_json::{Serializer, Value};
use std::io;

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

/// The shortest round-trip decimal digits of a finite, nonzero magnitude and the exponent of the first digit.
///
/// CPython takes, among the shortest candidates, the one nearest the exact value, and breaks an exact tie
/// toward the even digit. Rust's shortest form can break that tie the other way (`1743746592103460.25`
/// prints `...3`), so the digits come from the exact rounding to the same count when that rounding
/// round-trips; Rust's shortest form is the fallback.
fn shortest_digits(magnitude: f64) -> (String, i32) {
  let shortest = format!("{magnitude:e}");
  let (mantissa, _) = shortest.split_once('e').expect("LowerExp output carries an exponent");
  let count = mantissa.bytes().filter(u8::is_ascii_digit).count();
  let exact = format!("{magnitude:.*e}", count - 1);
  let text = if exact.parse::<f64>() == Ok(magnitude) { exact } else { shortest };
  let (mantissa, exponent) = text.split_once('e').expect("LowerExp output carries an exponent");
  let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
  let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
  (digits.trim_end_matches('0').to_string(), exponent)
}

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
  let negative = x < 0.0;
  let (digits, exponent) = shortest_digits(x.abs());
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

/// Python's `format(x, "g")` (`%g`): six significant digits with the trailing zeros dropped. The exponent form is used
/// when the exponent after rounding is below -4 or at least six, as C's `%g` chooses.
pub fn format_g(x: f64) -> String {
  const PRECISION: i32 = 6;
  if x.is_nan() {
    return "nan".to_string();
  }
  if x.is_infinite() {
    return if x > 0.0 { "inf" } else { "-inf" }.to_string();
  }
  if x == 0.0 {
    return if x.is_sign_negative() { "-0" } else { "0" }.to_string();
  }
  // The exponent after rounding to PRECISION significant digits, as C's %e reports it.
  let scientific = format!("{x:.*e}", (PRECISION - 1) as usize);
  let (mantissa, exponent) = scientific.split_once('e').expect("LowerExp output carries an exponent");
  let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
  if (-4..PRECISION).contains(&exponent) {
    let decimals = (PRECISION - 1 - exponent) as usize;
    trim_fraction_zeros(&format!("{x:.decimals$}"))
  } else {
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{}e{sign}{:02}", trim_fraction_zeros(mantissa), exponent.unsigned_abs())
  }
}

/// CPython's `'%.{precision}f' % x`, which `format(x, '.{precision}f')` shares: `x` in fixed notation with `precision`
/// digits after the point. Rust's fixed form rounds the exact binary value and breaks an exact tie toward the even
/// digit, as CPython does. NaN and the infinities print as CPython's lowercase words.
pub fn format_f(x: f64, precision: usize) -> String {
  if x.is_nan() {
    return "nan".to_string();
  }
  if x.is_infinite() {
    return if x > 0.0 { "inf" } else { "-inf" }.to_string();
  }
  format!("{x:.precision$}")
}

/// Drops the trailing zeros, and a bare decimal point, from a fixed-notation number.
fn trim_fraction_zeros(text: &str) -> String {
  if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text.to_string() }
}

/// pydantic's `model_dump_json` float form: the shortest round-trip digits in fixed notation when the decimal
/// exponent is in `-5..16`, and in scientific notation otherwise, whose exponent has neither `+` nor padding
/// (`1e16`, `1.5e-7`, `0.00001`). Checked against pydantic over random doubles; `repr` differs (`1e+16`).
/// `x` must be finite: `write_json` renders NaN and the infinities before a float reaches this function.
pub fn pydantic_float_repr(x: f64) -> String {
  if x == 0.0 {
    return if x.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
  }
  let (digits, exponent) = shortest_digits(x.abs());
  let body = if (-5..16).contains(&exponent) {
    if exponent >= 0 {
      let point = exponent as usize + 1;
      if digits.len() <= point {
        format!("{}{}.0", digits, "0".repeat(point - digits.len()))
      } else {
        format!("{}.{}", &digits[..point], &digits[point..])
      }
    } else {
      format!("0.{}{}", "0".repeat(exponent.unsigned_abs() as usize - 1), digits)
    }
  } else {
    let mut text = digits[..1].to_string();
    if digits.len() > 1 {
      text.push('.');
      text.push_str(&digits[1..]);
    }
    format!("{text}e{exponent}")
  };
  if x < 0.0 { format!("-{body}") } else { body }
}

/// CPython `repr(str)` for the strings this server reports (purposes, paths, config keys).
///
/// The quote is `'` unless the text holds `'` and no `"`. Backslash, the quote, `\t`, `\n` and `\r` are escaped,
/// and so is every character that `str.isprintable()` rejects, as `\xNN`, `\uNNNN` or `\UNNNNNNNN`. Every other
/// character prints as itself.
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
      c if !is_printable(c) => out.push_str(&repr_escape(c)),
      c => out.push(c),
    }
  }
  out.push(quote);
  out
}

/// `str.isprintable()` for one character: false exactly for the code points of `NON_PRINTABLE`.
fn is_printable(c: char) -> bool {
  let code = c as u32;
  let index = NON_PRINTABLE.partition_point(|&(_, high)| high < code);
  !NON_PRINTABLE.get(index).is_some_and(|&(low, _)| low <= code)
}

/// CPython's escape for one non-printable character: `\xNN` up to U+00FF, `\uNNNN` up to U+FFFF, else `\UNNNNNNNN`.
fn repr_escape(c: char) -> String {
  let code = c as u32;
  if code <= 0xff {
    format!("\\x{code:02x}")
  } else if code <= 0xffff {
    format!("\\u{code:04x}")
  } else {
    format!("\\U{code:08x}")
  }
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

/// How a float prints: `json.dumps` writes `repr(float)`, pydantic's `model_dump_json` its own form.
#[derive(Clone, Copy)]
enum FloatForm {
  Python,
  Pydantic,
}

/// `json.dumps(value, ...)` under `style`. Styles that pass non-ASCII through serialize through
/// serde_json (see [`serde_style_dumps`]); the `ensure_ascii` forms keep the hand escaper, whose
/// `\uXXXX` pairs no crate emits.
pub fn json_dumps(value: &Value, style: &JsonStyle) -> String {
  if style.ensure_ascii {
    let mut out = String::new();
    write_json(&mut out, value, style, FloatForm::Python, 0);
    out
  } else {
    serde_style_dumps(value, style, FloatForm::Python)
  }
}

/// `model_dump_json(...)` of a pydantic model under `style`: the `json_dumps` layout with floats in pydantic's form.
pub fn model_json_dumps(value: &Value, style: &JsonStyle) -> String {
  if style.ensure_ascii {
    let mut out = String::new();
    write_json(&mut out, value, style, FloatForm::Pydantic, 0);
    out
  } else {
    serde_style_dumps(value, style, FloatForm::Pydantic)
  }
}

/// `json.dumps` under `style` through serde_json's serializer: the crate's ESCAPE-table string scan
/// gives every string `json.dumps`'s escape set with non-ASCII passed through, and the container walk
/// is serde_json's. [`StyleFormatter`] supplies the forms serde_json lacks against Python: the
/// separators, the indent, and the float text.
fn serde_style_dumps(value: &Value, style: &JsonStyle, floats: FloatForm) -> String {
  let mut buffer = Vec::new();
  let serializer = &mut Serializer::with_formatter(
    &mut buffer,
    StyleFormatter { style: *style, floats, current_indent: 0, has_value: false },
  );
  // `sort_keys` is not a serde_json option, so only it takes the wrapping walk; the hot styles keep the
  // direct one.
  let serialized =
    if style.sort_keys { SortedValue { value }.serialize(serializer) } else { value.serialize(serializer) };
  serialized.expect("serializing a Value into a buffer cannot fail");
  String::from_utf8(buffer).expect("serde_json writes UTF-8")
}

/// Wraps a `Value` for serialization with every object's members in key order: `json.dumps`'s
/// `sort_keys`, which serde_json does not offer (its maps keep their order).
struct SortedValue<'a> {
  value: &'a Value,
}

impl Serialize for SortedValue<'_> {
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    match self.value {
      Value::Array(items) => serializer.collect_seq(items.iter().map(|item| SortedValue { value: item })),
      Value::Object(members) => {
        let mut entries: Vec<(&str, &Value)> = members.iter().map(|(key, value)| (key.as_str(), value)).collect();
        entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
        serializer.collect_map(entries.into_iter().map(|(key, value)| (key, SortedValue { value })))
      }
      other => other.serialize(serializer),
    }
  }
}

fn write_json(out: &mut String, value: &Value, style: &JsonStyle, floats: FloatForm, level: usize) {
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
          match floats {
            FloatForm::Python => float_repr(float),
            FloatForm::Pydantic => pydantic_float_repr(float),
          }
        });
      } else {
        out.push_str(&number.to_string());
      }
    }
    Value::String(text) => write_ascii_json_str(out, text),
    Value::Array(items) => {
      if items.is_empty() {
        out.push_str("[]");
        return;
      }
      out.push('[');
      for (index, item) in items.iter().enumerate() {
        write_separator(out, style, level + 1, index == 0);
        write_json(out, item, style, floats, level + 1);
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
        write_ascii_json_str(out, key);
        out.push_str(style.key_separator);
        write_json(out, item, style, floats, level + 1);
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

/// Appends one JSON string literal as `json.dumps(ensure_ascii=True)` writes it: the control set and the
/// two structural characters escaped, and every character above `~` as CPython's `\uXXXX` pairs.
///
/// This escaper stays hand-written because no crate emits the `\uXXXX` pairs: serde_json's scan passes
/// non-ASCII through (its ESCAPE table holds only the control set, quote and backslash), which is exactly
/// the form the styles that do not set `ensure_ascii` take through [`serde_style_dumps`].
pub fn write_ascii_json_str(out: &mut String, text: &str) {
  out.push('"');
  for character in text.chars() {
    match character {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      '\u{8}' => out.push_str("\\b"),
      '\u{c}' => out.push_str("\\f"),
      c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
      c if (c as u32) <= 0x7e => out.push(c),
      c => {
        let mut units = [0u16; 2];
        for unit in c.encode_utf16(&mut units) {
          out.push_str(&format!("\\u{unit:04x}"));
        }
      }
    }
  }
  out.push('"');
}

/// The serde_json `Formatter` of [`serde_style_dumps`]: Python's separators and indent between members,
/// and Python's or pydantic's float text. The `current_indent`/`has_value` pair tracks the nesting the
/// trait does not pass: the layout is `json.dumps`'s, with an indented style opening each member on its
/// own line and closing a non-empty container at its own level.
struct StyleFormatter {
  style: JsonStyle,
  floats: FloatForm,
  current_indent: usize,
  has_value: bool,
}

impl StyleFormatter {
  /// Before a member: the item separator between members, then the newline and indent of an indented style.
  fn member_prefix<W: ?Sized + io::Write>(&mut self, writer: &mut W, first: bool) -> io::Result<()> {
    if !first {
      writer.write_all(self.style.item_separator.as_bytes())?;
    }
    if let Some(indent) = self.style.indent {
      writer.write_all(b"\n")?;
      writer.write_all(" ".repeat(indent * self.current_indent).as_bytes())?;
    }
    Ok(())
  }

  /// Before a closing bracket: the container's own newline and indent, which an empty container skips.
  fn closing_prefix<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
    self.current_indent -= 1;
    if self.has_value
      && let Some(indent) = self.style.indent
    {
      writer.write_all(b"\n")?;
      writer.write_all(" ".repeat(indent * self.current_indent).as_bytes())?;
    }
    Ok(())
  }
}

impl JsonFormatter for StyleFormatter {
  fn write_f64<W>(&mut self, writer: &mut W, value: f64) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    let text = match self.floats {
      FloatForm::Python => float_repr(value),
      FloatForm::Pydantic => pydantic_float_repr(value),
    };
    writer.write_all(text.as_bytes())
  }

  fn begin_array<W>(&mut self, writer: &mut W) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    self.current_indent += 1;
    self.has_value = false;
    writer.write_all(b"[")
  }

  fn end_array<W>(&mut self, writer: &mut W) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    self.closing_prefix(writer)?;
    writer.write_all(b"]")
  }

  fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    self.member_prefix(writer, first)
  }

  fn end_array_value<W>(&mut self, _writer: &mut W) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    self.has_value = true;
    Ok(())
  }

  fn begin_object<W>(&mut self, writer: &mut W) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    self.current_indent += 1;
    self.has_value = false;
    writer.write_all(b"{")
  }

  fn end_object<W>(&mut self, writer: &mut W) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    self.closing_prefix(writer)?;
    writer.write_all(b"}")
  }

  fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    self.member_prefix(writer, first)
  }

  fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    writer.write_all(self.style.key_separator.as_bytes())
  }

  fn end_object_value<W>(&mut self, _writer: &mut W) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    self.has_value = true;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use std::path::Path;
  use std::process::Command;

  use super::*;
  use serde_json::json;

  #[test]
  fn float_digits_break_exact_ties_toward_the_even_digit() {
    // 1743746592103460.25 lies exactly between ...460.2 and ...460.3; CPython and pydantic print the even one.
    let tie = 1_743_746_592_103_460.0_f64 + 0.25;
    assert_eq!(float_repr(tie), "1743746592103460.2");
    assert_eq!(pydantic_float_repr(tie), "1743746592103460.2");
  }

  #[test]
  fn pydantic_floats_use_exponent_form_outside_the_fixed_range() {
    assert_eq!(pydantic_float_repr(1e16), "1e16");
    assert_eq!(pydantic_float_repr(1.5e-7), "1.5e-7");
    assert_eq!(pydantic_float_repr(1e-5), "0.00001");
    assert_eq!(pydantic_float_repr(-2.5), "-2.5");
  }

  #[test]
  fn format_g_matches_cpython_percent_g() {
    let cases = [
      (1.0, "1"),
      (59.5, "59.5"),
      (1.2583333, "1.25833"),
      (0.0001, "0.0001"),
      (0.00001, "1e-05"),
      (100000.0, "100000"),
      (1234567.0, "1.23457e+06"),
      (1e6, "1e+06"),
      (-2.5, "-2.5"),
    ];
    for (value, expected) in cases {
      assert_eq!(format_g(value), expected, "format({value}, 'g')");
    }
  }

  #[test]
  fn format_f_rounds_the_exact_binary_value_like_cpython() {
    let cases = [
      (0.125, 2, "0.12"),
      (0.375, 2, "0.38"),
      (2.675, 2, "2.67"),
      (0.5, 0, "0"),
      (1.5, 0, "2"),
      (2.5, 0, "2"),
      (-0.04, 1, "-0.0"),
      (1e20, 0, "100000000000000000000"),
      (f64::NAN, 2, "nan"),
      (f64::NEG_INFINITY, 1, "-inf"),
    ];
    for (value, precision, expected) in cases {
      assert_eq!(format_f(value, precision), expected, "'%.{precision}f' % {value}");
    }
  }

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

  /// The reference escaper: one char at a time, the form `json.dumps(ensure_ascii=True)` documents.
  fn reference_ascii_json_str(out: &mut String, text: &str) {
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
        c if (c as u32) <= 0x7e => out.push(c),
        c => {
          let mut units = [0u16; 2];
          for unit in c.encode_utf16(&mut units) {
            out.push_str(&format!("\\u{unit:04x}"));
          }
        }
      }
    }
    out.push('"');
  }

  #[test]
  fn ascii_json_str_matches_the_char_reference_over_every_byte_class() {
    // A deterministic corpus over all byte classes, at lengths that cross every code-point width.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state
    };
    let alphabet: Vec<char> = (0u32..0x2fff)
      .filter_map(char::from_u32)
      .chain(['\u{1f600}', '\u{7f}', '\u{1}', '\u{b}', '\u{c}', '"', '\\', '\n', 'a', ' ', '~'].map(|c| c))
      .collect();
    for length in 0..400 {
      let text: String = (0..length).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
      let mut fast = String::new();
      write_ascii_json_str(&mut fast, &text);
      let mut reference = String::new();
      reference_ascii_json_str(&mut reference, &text);
      assert_eq!(fast, reference, "length {length}");
    }
  }

  #[test]
  fn ascii_json_str_escapes_everything_above_the_tilde() {
    let mut escaped = String::new();
    write_ascii_json_str(&mut escaped, "quote\" back\\ nl\newline\r tab\t del\u{7f} \u{e9} \u{2014} \u{1f600}");
    assert_eq!(escaped, "\"quote\\\" back\\\\ nl\\newline\\r tab\\t del\\u007f \\u00e9 \\u2014 \\ud83d\\ude00\"");
  }

  #[test]
  fn starlette_style_matches_cpython_ensure_ascii_false() {
    // The bytes of `json.dumps(text, ensure_ascii=False)` in CPython: the control set escaped, DEL and
    // the non-ASCII characters raw (the shapes the fixture `json_renders.json` pins at large).
    assert_eq!(
      json_dumps(&json!("raw \u{e9} \u{2014} \u{1f600} \u{1} \u{b} del\u{7f}"), &STARLETTE_JSON),
      "\"raw \u{e9} \u{2014} \u{1f600} \\u0001 \\u000b del\u{7f}\""
    );
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

  /// Prints `repr` of every code point that `char` holds, one per line. The surrogates are not `char`s, so
  /// both sides skip them.
  const CPYTHON_REPRS: &str = r#"
import sys
for code_point in range(0x110000):
    if 0xD800 <= code_point <= 0xDFFF:
        continue
    sys.stdout.buffer.write((repr(chr(code_point)) + "\n").encode("utf-8"))
"#;

  #[test]
  fn str_repr_matches_cpython_for_every_code_point() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).expect("the crate lies under the repo");
    let output = Command::new("uv")
      .args(["run", "python", "-c", CPYTHON_REPRS])
      .current_dir(repo_root)
      .output()
      .expect("uv runs the worktree Python");
    assert!(output.status.success(), "CPython repr failed:\n{}", String::from_utf8_lossy(&output.stderr));
    let reprs = String::from_utf8(output.stdout).expect("CPython prints UTF-8");
    let reprs: Vec<&str> = reprs.split_terminator('\n').collect();
    let code_points: Vec<char> = (0..=0x10_ffff_u32).filter_map(char::from_u32).collect();
    assert_eq!(reprs.len(), code_points.len(), "CPython prints one repr per code point");
    let mismatches: Vec<String> = code_points
      .iter()
      .zip(&reprs)
      .filter_map(|(c, cpython)| {
        let ours = str_repr(&c.to_string());
        (ours != *cpython).then(|| format!("U+{:04X}: CPython {cpython}, str_repr {ours}", *c as u32))
      })
      .collect();
    let first = &mismatches[..mismatches.len().min(8)];
    assert!(mismatches.is_empty(), "{} code points differ, first {first:?}", mismatches.len());
  }
}
