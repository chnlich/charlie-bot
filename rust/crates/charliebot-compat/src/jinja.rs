//! The Jinja2 environment the Python app renders its pages with, as minijinja configures it.
//!
//! Starlette's `Jinja2Templates` builds a Jinja2 environment with autoescaping, and a page prints values
//! through it: markup prints as is, anything else prints as Python's `str()` escaped with markupsafe's table.
//! Two filters carry bytes the pages depend on, so they are Jinja2's own: `tojson` (`htmlsafe_json_dumps` with
//! `sort_keys`) and `urlencode` (`do_urlencode`). minijinja's `tojson` and `urlencode` are replaced here.

use std::path::PathBuf;

use charliebot_core::pyfmt;
use minijinja::value::ValueKind;
use minijinja::{AutoEscape, Environment, Error, ErrorKind, Output, State, Value};

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
    _ => pyfmt::python_str(&to_json(value)?),
  })
}

fn to_json(value: &Value) -> Result<serde_json::Value, Error> {
  serde_json::to_value(value).map_err(|error| Error::new(ErrorKind::BadSerialization, error.to_string()))
}

/// Jinja2's `tojson` (`htmlsafe_json_dumps` with `sort_keys=True`): `json.dumps` text with `< > & '`
/// escaped as JSON unicode escapes, marked safe.
fn tojson(value: Value) -> Result<Value, Error> {
  if value.is_undefined() {
    return Err(Error::new(ErrorKind::BadSerialization, "Object of type Undefined is not JSON serializable"));
  }
  let text = pyfmt::json_dumps(&to_json(&value)?, &pyfmt::JINJA_TOJSON)
    .replace('<', "\\u003c")
    .replace('>', "\\u003e")
    .replace('&', "\\u0026")
    .replace('\'', "\\u0027");
  Ok(Value::from_safe_string(text))
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
    _ => Ok(quote(&python_text(&value)?, b"/")),
  }
}

/// `url_quote(key, for_qs=True)=url_quote(value, for_qs=True)`, one pair of a query string.
fn query_pair(key: &Value, value: &Value) -> Result<String, Error> {
  Ok(format!("{}={}", query_quote(&python_text(key)?), query_quote(&python_text(value)?)))
}

/// Jinja2's `url_quote(text, for_qs=True)`: `urllib.parse.quote` with no safe characters, then `%20` as `+`.
fn query_quote(text: &str) -> String {
  quote(text, b"").replace("%20", "+")
}

/// `urllib.parse.quote(text, safe)`: the unreserved characters and `safe` pass, every other byte is `%XX`.
fn quote(text: &str, safe: &[u8]) -> String {
  let mut out = String::with_capacity(text.len());
  for byte in text.bytes() {
    if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) || safe.contains(&byte) {
      out.push(byte as char);
    } else {
      out.push_str(&format!("%{byte:02X}"));
    }
  }
  out
}

#[cfg(test)]
mod tests {
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
  fn urlencode_keeps_slashes_for_a_scalar_and_joins_pairs_for_a_mapping() {
    assert_eq!(render_str("{{ v|urlencode }}", json!({"v": "a b/\u{b7}~"})), "a%20b/%C2%B7~");
    assert_eq!(urlencode(Value::from_serialize(json!([["a", 1], ["b c", "x/y"]]))).unwrap(), "a=1&b+c=x%2Fy");
    assert_eq!(urlencode(Value::from_serialize(json!({"k": "v w", "n": null}))).unwrap(), "k=v+w&n=None");
    assert_eq!(urlencode(Value::UNDEFINED).unwrap(), "");
  }

  #[test]
  fn datetime_isoformat_returns_the_serialized_timestamp() {
    let timestamp = "2026-10-18T12:30:00+00:00";
    assert_eq!(render_str("{{ timestamp.isoformat() }}", json!({"timestamp": timestamp})), timestamp);
  }
}
