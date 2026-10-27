//! The JSON bodies the Python server writes: orjson's `dumps` (`src/infra/responses.py`) and Starlette's
//! `JSONResponse.render`.
//!
//! The two renderings differ only in float text. orjson writes the shortest round-trip digits in plain
//! notation when the decimal exponent is -5 to 15, and in `e+N` or `e-N` notation outside that range,
//! with no exponent padding. CPython's `repr`, which `json.dumps` uses, switches at -4 and 16 and pads
//! the exponent to two digits. Strings, containers and integers render the same in both.

use charliebot_core::pyfmt;
use serde::Serialize;
use serde_json::ser::Formatter as JsonFormatter;
use serde_json::{Serializer, Value};
use std::io;

/// orjson's `dumps(value)`: compact separators, members in insertion order, raw UTF-8.
///
/// The bytes come from `serde_json`'s serializer: its ESCAPE-table string scan and container walk are
/// orjson's own semantics for strings and containers, and [`OrjsonFormatter`] supplies the one form
/// serde_json lacks, orjson's float text.
pub fn fast_json_bytes(value: &Value) -> Vec<u8> {
  let mut buffer = Vec::new();
  value
    .serialize(&mut Serializer::with_formatter(&mut buffer, OrjsonFormatter))
    .expect("serializing a Value into a buffer cannot fail");
  buffer
}

/// Starlette's `JSONResponse.render(value)`: `json.dumps(value, ensure_ascii=False, separators=(",", ":"))`.
pub fn starlette_bytes(value: &Value) -> Vec<u8> {
  pyfmt::json_dumps(value, &pyfmt::STARLETTE_JSON).into_bytes()
}

/// The formatter that adds orjson's float text to serde_json's writer.
struct OrjsonFormatter;

impl JsonFormatter for OrjsonFormatter {
  fn write_f64<W>(&mut self, writer: &mut W, value: f64) -> io::Result<()>
  where
    W: ?Sized + io::Write,
  {
    writer.write_all(orjson_float(value).as_bytes())
  }
}

/// orjson's float text for a finite `value`. serde_json holds no NaN or infinity, so none reach here.
fn orjson_float(value: f64) -> String {
  if value == 0.0 {
    return if value.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
  }
  // `{:e}` prints the shortest round-trip digits, the digits CPython's repr also picks: `-1.5e-7`, `1e16`.
  let scientific = format!("{value:e}");
  let (mantissa, exponent) = scientific.split_once('e').expect("LowerExp output carries an exponent");
  let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
  let (sign, mantissa) = match mantissa.strip_prefix('-') {
    Some(magnitude) => ("-", magnitude),
    None => ("", mantissa),
  };
  let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
  let body = if (-5..=15).contains(&exponent) {
    plain_notation(&digits, exponent)
  } else {
    let (first, rest) = digits.split_at(1);
    let fraction = if rest.is_empty() { String::new() } else { format!(".{rest}") };
    let exponent_sign = if exponent < 0 { '-' } else { '+' };
    format!("{first}{fraction}e{exponent_sign}{}", exponent.unsigned_abs())
  };
  format!("{sign}{body}")
}

/// The digits `d1 d2 ...` read as `d1.d2... x 10^exponent`, in plain notation; an integral value ends in `.0`.
fn plain_notation(digits: &str, exponent: i32) -> String {
  if exponent < 0 {
    return format!("0.{}{digits}", "0".repeat((-exponent - 1) as usize));
  }
  let integer_digits = exponent as usize + 1;
  if digits.len() <= integer_digits {
    format!("{digits}{}.0", "0".repeat(integer_digits - digits.len()))
  } else {
    format!("{}.{}", &digits[..integer_digits], &digits[integer_digits..])
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn orjson_floats_switch_notation_at_exponent_minus_five_and_sixteen() {
    let cases = [
      (1e-5, "0.00001"),
      (1.5e-7, "1.5e-7"),
      (1e16, "1e+16"),
      (9999999999999998.0, "9999999999999998.0"),
      (-0.0, "-0.0"),
      (100.0, "100.0"),
    ];
    for (value, expected) in cases {
      assert_eq!(orjson_float(value), expected, "orjson of {value:e}");
    }
  }

  #[test]
  fn containers_keep_insertion_order_and_raw_utf8() {
    let value = json!({"b": [null, 1.5], "a": "é"});
    assert_eq!(String::from_utf8(fast_json_bytes(&value)).unwrap(), r#"{"b":[null,1.5],"a":"é"}"#);
  }
}
