//! The Jinja2 environment the Python app renders its pages with, as minijinja configures it.
//!
//! Starlette's `Jinja2Templates` builds a Jinja2 environment with autoescaping, and a page prints values
//! through it: markup prints as is, anything else prints as Python's `str()` escaped with markupsafe's table.
//! Three filters carry bytes the pages depend on, so they are Jinja2's own. They are `tojson`
//! (`htmlsafe_json_dumps` with `sort_keys`), `urlencode` (`do_urlencode`) and `format` (`do_format` with one
//! positional argument, the `%` operator on the value). minijinja's `tojson`, `urlencode` and `format` are replaced
//! here.

use std::path::PathBuf;

use charliebot_core::text_repr;
use minijinja::value::ValueKind;
use minijinja::{AutoEscape, Environment, Error, ErrorKind, Output, State, Value};
use percent_encoding::{AsciiSet, utf8_percent_encode};

use crate::urlquote::quote_set;

/// A loader that serves each name from the first of `directories` that holds it, as Jinja2's
/// `FileSystemLoader` does.
pub fn file_system_loader(
  directories: Vec<PathBuf>,
) -> impl Fn(&str) -> Result<Option<String>, Error> + Send + Sync + 'static {
  move |name| load_template(&directories, name)
}

/// An environment that loads templates through `loader`, with the autoescaping, printing and filters above.
pub fn environment(
  loader: impl Fn(&str) -> Result<Option<String>, Error> + Send + Sync + 'static,
) -> Environment<'static> {
  let mut environment = Environment::new();
  environment.set_loader(loader);
  environment.set_auto_escape_callback(|_| AutoEscape::Html);
  environment.set_formatter(markupsafe_formatter);
  environment.set_unknown_method_callback(|state, value, method, args| {
    // Page contexts keep Python's datetime.isoformat() call after serializing datetimes as ISO strings.
    if method == "isoformat" && value.kind() == ValueKind::String && args.is_empty() {
      return Ok(value.clone());
    }
    minijinja_contrib::pycompat::unknown_method_callback(state, value, method, args)
  });
  environment.add_filter("tojson", tojson);
  environment.add_filter("urlencode", urlencode);
  environment.add_filter("format", percent_format);
  environment
}

/// Jinja2 `FileSystemLoader.get_source`: the first search directory holding the name wins; a `..`
/// segment names no template. The text is read as UTF-8 with universal newlines.
fn load_template(directories: &[PathBuf], name: &str) -> Result<Option<String>, Error> {
  let mut pieces = Vec::new();
  for piece in name.split('/') {
    if piece == ".." {
      return Ok(None);
    }
    if !piece.is_empty() && piece != "." {
      pieces.push(piece);
    }
  }
  for directory in directories {
    let path = pieces.iter().fold(directory.clone(), |path, piece| path.join(piece));
    if path.is_file() {
      let bytes = std::fs::read(&path).map_err(|error| {
        Error::new(ErrorKind::InvalidOperation, format!("reading template {}: {error}", path.display()))
      })?;
      let text = String::from_utf8(bytes).map_err(|error| {
        Error::new(ErrorKind::InvalidOperation, format!("UnicodeDecodeError: template {}: {error}", path.display()))
      })?;
      return Ok(Some(text.replace("\r\n", "\n").replace('\r', "\n")));
    }
  }
  Ok(None)
}

/// Print a value as Jinja2 does under autoescaping: markup prints as is; anything else prints as
/// Python's `str()` of it, escaped with markupsafe's table.
fn markupsafe_formatter(out: &mut Output, state: &State, value: &Value) -> Result<(), Error> {
  if value.is_safe() || state.auto_escape() == AutoEscape::None {
    return out.write_str(&python_text(value)?).map_err(Error::from);
  }
  out.write_str(&markupsafe_escape(&python_text(value)?)).map_err(Error::from)
}

/// `markupsafe.escape`: `& < > ' "` as `&amp; &lt; &gt; &#39; &#34;`.
fn markupsafe_escape(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  for c in text.chars() {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '\'' => out.push_str("&#39;"),
      '"' => out.push_str("&#34;"),
      c => out.push(c),
    }
  }
  out
}

/// Python's `str()` of a template value; an undefined value prints as nothing (Jinja2 `Undefined`).
fn python_text(value: &Value) -> Result<String, Error> {
  Ok(match value.kind() {
    ValueKind::Undefined => String::new(),
    ValueKind::String => value.as_str().expect("a string value has text").to_string(),
    _ => text_repr::python_str(&to_json(value)?),
  })
}

fn to_json(value: &Value) -> Result<serde_json::Value, Error> {
  serde_json::to_value(value).map_err(|error| Error::new(ErrorKind::BadSerialization, error.to_string()))
}

/// `htmlsafe_json_dumps`'s escaping of the JSON text: `< > & '` as unicode escapes.
///
/// One scan that copies the runs between matches; four `str::replace` calls would each scan the whole
/// text. A text with no match keeps its buffer.
fn html_safe_json(json: String) -> String {
  fn escape(byte: u8) -> Option<&'static str> {
    match byte {
      b'<' => Some("\\u003c"),
      b'>' => Some("\\u003e"),
      b'&' => Some("\\u0026"),
      b'\'' => Some("\\u0027"),
      _ => None,
    }
  }
  let bytes = json.as_bytes();
  let Some(first) = bytes.iter().position(|byte| escape(*byte).is_some()) else { return json };
  let mut out = String::with_capacity(json.len() + 16);
  let mut copied = 0;
  for (index, byte) in bytes.iter().enumerate().skip(first) {
    if let Some(replacement) = escape(*byte) {
      out.push_str(&json[copied..index]);
      out.push_str(replacement);
      copied = index + 1;
    }
  }
  out.push_str(&json[copied..]);
  out
}

/// Jinja2's `tojson` (`htmlsafe_json_dumps` with `sort_keys=True`): `json.dumps` text with `< > & '`
/// escaped as JSON unicode escapes, marked safe.
fn tojson(value: Value) -> Result<Value, Error> {
  if value.is_undefined() {
    return Err(Error::new(ErrorKind::BadSerialization, "Object of type Undefined is not JSON serializable"));
  }
  Ok(Value::from_safe_string(tojson_text(&to_json(&value)?)))
}

/// The text [`tojson`] renders: `json.dumps(sort_keys=True)` with the `htmlsafe_json_dumps` escapes.
///
/// A page that carries a large value as text renders this once in the handler. The filter's path walks the
/// value into the template engine's value tree and back out again, which costs one pass per element.
pub fn tojson_text(value: &serde_json::Value) -> String {
  html_safe_json(text_repr::json_dumps(value, &text_repr::JINJA_TOJSON))
}

/// Jinja2's `do_urlencode`. A scalar's text is quoted with `/` kept (`urllib.parse.quote`). A mapping, or a
/// sequence of `(key, value)` pairs, joins `key=value` with `&`, each side quoted for a query string: `/` is
/// quoted and a space is `+`. An undefined value is empty, since Jinja2's `Undefined` iterates as nothing.
fn urlencode(value: Value) -> Result<String, Error> {
  match value.kind() {
    ValueKind::Undefined => Ok(String::new()),
    ValueKind::Map => {
      let mut pairs = Vec::new();
      for key in value.try_iter()? {
        let item = value.get_item(&key)?;
        pairs.push(query_pair(&key, &item)?);
      }
      Ok(pairs.join("&"))
    }
    ValueKind::Seq => {
      let mut pairs = Vec::new();
      for pair in value.try_iter()? {
        let items: Vec<Value> = match pair.kind() {
          ValueKind::Seq => pair.try_iter()?.collect(),
          _ => Vec::new(),
        };
        let [key, item] = items.as_slice() else {
          return Err(Error::new(ErrorKind::InvalidOperation, "urlencode takes (key, value) pairs"));
        };
        pairs.push(query_pair(key, item)?);
      }
      Ok(pairs.join("&"))
    }
    _ => Ok(quote(&python_text(&value)?, &URLENCODE_QUOTE)),
  }
}

/// `url_quote(key, for_qs=True)=url_quote(value, for_qs=True)`, one pair of a query string.
fn query_pair(key: &Value, value: &Value) -> Result<String, Error> {
  Ok(format!("{}={}", query_quote(&python_text(key)?), query_quote(&python_text(value)?)))
}

/// Jinja2's `url_quote(text, for_qs=True)`: `urllib.parse.quote` with no safe characters, then `%20` as `+`.
fn query_quote(text: &str) -> String {
  quote(text, &QUERY_QUOTE).replace("%20", "+")
}

/// Jinja2's `format` filter (`do_format`): `soft_str(value) % (kwargs or args)` with the one positional argument the
/// pages pass. The conversions are `%f` and `%.Nf`, `%d`, `%s` and `%%`; any other conversion fails the render.
fn percent_format(template: Value, argument: Value) -> Result<String, Error> {
  let template = python_text(&template)?;
  let argument = to_json(&argument)?;
  let mut out = String::with_capacity(template.len());
  let mut pending = Some(&argument);
  let mut rest = template.as_str();
  while let Some(percent) = rest.find('%') {
    out.push_str(&rest[..percent]);
    rest = &rest[percent + 1..];
    if let Some(after) = rest.strip_prefix('%') {
      out.push('%');
      rest = after;
      continue;
    }
    let spec_length = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    let spec = &rest[..spec_length];
    let Some(conversion) = rest[spec_length..].chars().next() else {
      return Err(format_error(format!("incomplete format {template:?}: the template ends in a %")));
    };
    rest = &rest[spec_length + conversion.len_utf8()..];
    let Some(value) = pending.take() else {
      return Err(format_error(format!("not enough arguments for the format string {template:?}")));
    };
    out.push_str(&convert(spec, conversion, value)?);
  }
  out.push_str(rest);
  if pending.is_some() {
    return Err(format_error(format!("not all arguments converted during the formatting of {template:?}")));
  }
  Ok(out)
}

/// One conversion of `percent_format`. `%f` takes `.N` (six digits when the spec is empty); `%d` and `%s` take no spec.
fn convert(spec: &str, conversion: char, value: &serde_json::Value) -> Result<String, Error> {
  match conversion {
    'f' => Ok(text_repr::format_f(real(value, conversion)?, precision(spec, conversion)?)),
    'd' if spec.is_empty() => integer(value),
    's' if spec.is_empty() => Ok(text_repr::python_str(value)),
    _ => Err(format_error(format!(
      "unsupported format conversion '%{spec}{conversion}': the format filter supports %f, %.Nf, %d, %s and %%"
    ))),
  }
}

/// The digits after the point of a `%f` spec: `.N`, `.` alone for zero, or nothing for Python's default of six.
fn precision(spec: &str, conversion: char) -> Result<usize, Error> {
  match spec.strip_prefix('.') {
    None if spec.is_empty() => Ok(6),
    None => Err(format_error(format!("unsupported format conversion '%{spec}{conversion}'"))),
    Some("") => Ok(0),
    Some(digits) => {
      digits.parse().map_err(|_| format_error(format!("unsupported format conversion '%{spec}{conversion}'")))
    }
  }
}

/// The argument of a `%f` conversion as a real number: an int or a float, and a bool as 0 or 1.
fn real(value: &serde_json::Value, conversion: char) -> Result<f64, Error> {
  match value {
    serde_json::Value::Number(number) => Ok(number.as_f64().expect("a serde_json number reads as f64")),
    serde_json::Value::Bool(flag) => Ok(f64::from(u8::from(*flag))),
    _ => Err(format_error(format!("%{conversion} format: a real number is required"))),
  }
}

/// The argument of a `%d` conversion as an integer: a float truncates toward zero, as `int()` does.
fn integer(value: &serde_json::Value) -> Result<String, Error> {
  match value {
    serde_json::Value::Number(number) if number.is_f64() => {
      let float = number.as_f64().expect("a serde_json number reads as f64");
      if !float.is_finite() {
        return Err(format_error(format!("cannot convert float {float} to integer")));
      }
      // The sign of a zero truncation is dropped: `int(-0.5)` is `0`, not `-0`.
      let truncated = float.trunc();
      Ok(format!("{:.0}", if truncated == 0.0 { 0.0 } else { truncated }))
    }
    serde_json::Value::Number(number) => Ok(number.to_string()),
    serde_json::Value::Bool(flag) => Ok(u8::from(*flag).to_string()),
    _ => Err(format_error("%d format: a number is required".to_string())),
  }
}

fn format_error(detail: String) -> Error {
  Error::new(ErrorKind::InvalidOperation, detail)
}

/// `urllib.parse.quote(text, "/")`, Jinja2's `do_urlencode` for a scalar.
static URLENCODE_QUOTE: AsciiSet = quote_set(b"/");
/// `urllib.parse.quote(text, "")`, the key and value sides of `url_quote(for_qs=True)`.
static QUERY_QUOTE: AsciiSet = quote_set(b"");

/// `urllib.parse.quote(text, safe)`: the unreserved characters and `safe` pass, every other byte is `%XX`.
fn quote(text: &str, safe: &'static AsciiSet) -> String {
  utf8_percent_encode(text, safe).to_string()
}

#[cfg(test)]
mod tests {
  use serde_json::Value as JsonValue;
  use serde_json::json;

  use super::*;

  fn render_str(source: &str, context: serde_json::Value) -> String {
    environment(|_| Ok(None)).render_str(source, Value::from_serialize(&context)).unwrap()
  }

  #[test]
  fn printing_follows_jinja2_under_autoescape() {
    let context = json!({"text": "<a href='x'>&\"/", "none": null, "yes": true, "number": 1.5, "items": ["a", 1]});
    assert_eq!(
      render_str("{{ text }}|{{ none }}|{{ yes }}|{{ number }}|{{ items }}|{{ missing }}", context),
      "&lt;a href=&#39;x&#39;&gt;&amp;&#34;/|None|True|1.5|[&#39;a&#39;, 1]|"
    );
  }

  #[test]
  fn tojson_matches_htmlsafe_json_dumps() {
    let context = json!({"value": {"b": "</script>&'", "a": [true, null, "\u{b7}"]}});
    assert_eq!(
      render_str("{{ value|tojson }}", context),
      concat!(r#"{"a": [true, null, ""#, "\\u00b7", r#""], "b": ""#, "\\u003c/script\\u003e\\u0026\\u0027", r#""}"#)
    );
  }

  #[test]
  fn html_safe_json_matches_the_four_replace_passes_on_every_input_class() {
    // One input per class the escaping discriminates: no match, each match, and matches beside ASCII,
    // control characters, quotes, backslashes and non-ASCII text.
    let inputs = [
      "",
      "plain ascii",
      "<script>alert('&')</script>",
      "\u{0}\u{1}\t\n\u{8}\r\u{1f}\"\\",
      "caf\u{e9} \u{2014} \u{4e2d}\u{6587} \u{1f600}",
      "<>&'<>&'",
      "\u{4e2d}<a href=\"x\">&'\u{1f600}\\",
    ];
    for text in inputs {
      let old = text.replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026").replace('\'', "\\u0027");
      assert_eq!(html_safe_json(text.to_string()), old, "{text:?}");
    }
  }

  #[test]
  fn urlencode_keeps_slashes_for_a_scalar_and_joins_pairs_for_a_mapping() {
    assert_eq!(render_str("{{ v|urlencode }}", json!({"v": "a b/\u{b7}~"})), "a%20b/%C2%B7~");
    assert_eq!(urlencode(Value::from_serialize(json!([["a", 1], ["b c", "x/y"]]))).unwrap(), "a=1&b+c=x%2Fy");
    assert_eq!(urlencode(Value::from_serialize(json!({"k": "v w", "n": null}))).unwrap(), "k=v+w&n=None");
    assert_eq!(urlencode(Value::UNDEFINED).unwrap(), "");
  }

  #[test]
  fn quote_and_query_quote_match_urllib_quote_for_their_safe_sets() {
    assert_eq!(quote("a:b@c=d&e/f_g.h-i~j", &URLENCODE_QUOTE), "a%3Ab%40c%3Dd%26e/f_g.h-i~j");
    assert_eq!(quote("[]!$&'()*+,;:%#?=@", &QUERY_QUOTE), "%5B%5D%21%24%26%27%28%29%2A%2B%2C%3B%3A%25%23%3F%3D%40");
    assert_eq!(query_quote("a b+c/\u{e9}"), "a+b%2Bc%2F%C3%A9");
    assert_eq!(quote("\u{7f}\n", &QUERY_QUOTE), "%7F%0A");
  }

  #[test]
  fn datetime_isoformat_returns_the_serialized_timestamp() {
    let timestamp = "2026-10-18T12:30:00+00:00";
    assert_eq!(render_str("{{ timestamp.isoformat() }}", json!({"timestamp": timestamp})), timestamp);
  }

  const JINJA2_FIXTURE: &str = include_str!("../tests/fixtures/jinja2_format_reference.json");

  fn jinja2_renders(cases: &[(String, serde_json::Value)]) -> Vec<String> {
    let fixture: JsonValue = serde_json::from_str(JINJA2_FIXTURE).unwrap();
    assert_eq!(fixture["source"], "rust/crates/infra/compat/src/jinja.rs::JINJA2_RENDER");
    assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
    let recorded_cases: Vec<(String, serde_json::Value)> = serde_json::from_value(fixture["cases"].clone()).unwrap();
    assert_eq!(recorded_cases, cases);
    serde_json::from_value(fixture["expected"].clone()).unwrap()
  }

  #[test]
  fn format_filter_matches_jinja2_for_the_page_and_the_edge_values() {
    let mut cases: Vec<(String, serde_json::Value)> = vec![
      // The uses in token_usage.html: `"%.1f"|format(cache_share)` at lines 85, 106 and 121.
      (r#"{{ "%.1f"|format(cache_share) }}%"#.into(), json!({"cache_share": 37.45})),
      (r#"{{ "%.2f"|format(ctx.out_share*100) }}% of the total"#.into(), json!({"ctx": {"out_share": 0.0625}})),
      (r#"{{ "%.1f"|format(ctx.fresh_percent) }}%"#.into(), json!({"ctx": {"fresh_percent": 12.25}})),
      (r#"{{"%.0f"|format(ps["share"])}}%"#.into(), json!({"ps": {"share": 2.5}})),
      (r#"{{ "%.2f"|format(ctx.elapsed_s) }}"#.into(), json!({"ctx": {"elapsed_s": 0.125}})),
      (r#"{{ "%d%%"|format(5) }} {{ "%.1f%%"|format(12.25) }}"#.into(), json!({})),
      (r#"{{ "%d"|format(x) }} {{ "%s"|format(x) }}"#.into(), json!({"x": 2.7})),
      (r#"{{ "%d"|format(x) }} {{ "%s"|format(x) }}"#.into(), json!({"x": -0.5})),
      (r#"{{ "%d"|format(x) }} {{ "%s"|format(x) }}"#.into(), json!({"x": true})),
      (r#"{{ "%d"|format(x) }} {{ "%s"|format(x) }}"#.into(), json!({"x": 7})),
      (r#"{{ "%s"|format(x) }}"#.into(), json!({"x": null})),
      (r#"{{ "%s"|format(x) }}"#.into(), json!({"x": "text"})),
    ];
    // Exact ties at two decimals (k/8), values at one and two decimals (k/1000), and halves at none (k/2).
    for k in 0..1000 {
      let eighth = f64::from(k) / 8.0;
      let thousandth = f64::from(k) / 1000.0;
      cases.push((r#"{{ "%.2f"|format(x) }}"#.into(), json!({"x": eighth})));
      cases.push((r#"{{ "%.2f"|format(x) }}"#.into(), json!({"x": thousandth})));
      cases.push((r#"{{ "%.1f"|format(x) }}"#.into(), json!({"x": thousandth})));
    }
    for k in 0..200 {
      cases.push((r#"{{ "%.0f"|format(x) }}"#.into(), json!({"x": f64::from(k) / 2.0})));
    }
    let expected = jinja2_renders(&cases);
    assert_eq!(expected.len(), cases.len());
    let environment = environment(|_| Ok(None));
    let mismatches: Vec<String> = cases
      .iter()
      .zip(&expected)
      .filter_map(|((template, context), jinja2)| {
        let ours = environment
          .render_str(template, Value::from_serialize(context))
          .unwrap_or_else(|error| panic!("{template} with {context}: {error}"));
        (ours != *jinja2).then(|| format!("{template} with {context}: Jinja2 {jinja2:?}, ours {ours:?}"))
      })
      .collect();
    assert!(
      mismatches.is_empty(),
      "{} renders differ, first {:?}",
      mismatches.len(),
      &mismatches[..mismatches.len().min(8)]
    );
  }

  #[test]
  fn format_filter_rounds_the_half_way_values_to_even() {
    assert_eq!(render_str(r#"{{ "%.2f"|format(0.125) }}"#, json!({})), "0.12");
    assert_eq!(render_str(r#"{{ "%.2f"|format(2.675) }}"#, json!({})), "2.67");
  }

  #[test]
  fn format_filter_fails_the_render_on_a_conversion_it_does_not_support() {
    for (template, named) in [(r#"{{ "%x"|format(10) }}"#, "'%x'"), (r#"{{ "%5.2f"|format(1.5) }}"#, "'%5.2f'")] {
      let error = environment(|_| Ok(None)).render_str(template, json!({})).expect_err("the conversion is refused");
      assert!(error.to_string().contains(named), "{template}: {error}");
    }
  }

  #[test]
  fn format_filter_refuses_an_argument_count_that_python_refuses() {
    let environment = environment(|_| Ok(None));
    let missing = environment.render_str(r#"{{ "%s %s"|format(1) }}"#, json!({})).expect_err("one argument is short");
    assert!(missing.to_string().contains("not enough arguments"), "{missing}");
    let unused = environment.render_str(r#"{{ "%%"|format(1) }}"#, json!({})).expect_err("the argument is unused");
    assert!(unused.to_string().contains("not all arguments converted"), "{unused}");
  }
}
