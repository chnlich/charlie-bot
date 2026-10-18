//! `http.cookies.SimpleCookie().load(header)` (CPython 3.14), reduced to reading one cookie's value.
//!
//! The auth gate reads the access-key cookie through `SimpleCookie`, whose parse rules decide which
//! headers carry the cookie at all: a malformed segment ends parsing, a reserved attribute before any
//! cookie or a value-less non-flag attribute discards the whole header, and an illegal cookie name
//! raises `CookieError`. Each of those outcomes means "no cookie" to the gate, so this module returns
//! the value only when CPython's jar would hold it.

use std::sync::LazyLock;

use regex::Regex;

/// `http.cookies._CookiePattern` with `re.ASCII` classes spelled out (`\w` = `[A-Za-z0-9_]`, `\s` =
/// `[ \t\n\r\x0B\x0C]`), applied at the start of the remaining text as `re.match` applies it.
static COOKIE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
  let space = r"[ \t\n\r\x0B\x0C]";
  let key_chars = r"A-Za-z0-9_!#%&'~`><@,:/$*+\-.^|)(?}{=";
  let pattern = format!(
    r#"^{space}*(?P<key>[{key_chars}]+?)(?:{space}*={space}*(?P<val>"(?:[^\\"]|\\.)*"|(?:[A-Za-z0-9_]{{3,6}}day|[A-Za-z0-9_]{{3}}),{space}[A-Za-z0-9_{space_chars}-]{{9,11}}{space}[0-9:]{{8}}{space}GMT|[{key_chars}\[\]]*))?{space}*(?:{space}+|;|\z)"#,
    space_chars = r" \t\n\r\x0B\x0C",
  );
  Regex::new(&pattern).expect("the cookie pattern compiles")
});

/// `Morsel._reserved`: attribute names that attach to the preceding cookie.
const RESERVED: &[&str] =
  &["expires", "path", "comment", "domain", "max-age", "secure", "httponly", "version", "samesite", "partitioned"];

/// `Morsel._flags`: reserved attributes that may appear without a value.
const FLAGS: &[&str] = &["secure", "httponly", "partitioned"];

/// `http.cookies._LegalChars`: the characters a cookie name may hold.
fn is_legal_key(key: &str) -> bool {
  !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~:".contains(&b))
}

/// `SimpleCookie` raised `CookieError` while applying the parsed header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieError(pub String);

enum Item {
  Attribute(String),
  Cookie(String, String),
}

/// The value of cookie `name` in a `Cookie` header, as `SimpleCookie().load(header)[name].value` reads it.
///
/// `Ok(None)` when the jar holds no such cookie, including a header CPython discards as invalid;
/// `Err` when loading raises `CookieError`.
pub fn cookie_value(header: &str, name: &str) -> Result<Option<String>, CookieError> {
  let mut items = Vec::new();
  let mut morsel_seen = false;
  let mut position = 0;
  while position < header.len() {
    let Some(captures) = COOKIE_PATTERN.captures(&header[position..]) else {
      break;
    };
    let key = captures.name("key").expect("the key group always participates").as_str();
    let value = captures.name("val").map(|value| value.as_str());
    position += captures.get(0).expect("group 0 is the match").end();
    if let Some(attribute) = key.strip_prefix('$') {
      if morsel_seen {
        items.push(Item::Attribute(attribute.to_string()));
      }
    } else if RESERVED.contains(&key.to_ascii_lowercase().as_str()) {
      if !morsel_seen {
        return Ok(None);
      }
      if value.is_none() && !FLAGS.contains(&key.to_ascii_lowercase().as_str()) {
        return Ok(None);
      }
      items.push(Item::Attribute(key.to_string()));
    } else if let Some(value) = value {
      items.push(Item::Cookie(key.to_string(), unquote(value)));
      morsel_seen = true;
    } else {
      return Ok(None);
    }
  }
  let mut found = None;
  for item in items {
    match item {
      Item::Attribute(attribute) => {
        if !RESERVED.contains(&attribute.to_ascii_lowercase().as_str()) {
          return Err(CookieError(format!("Invalid attribute {attribute:?}")));
        }
      }
      Item::Cookie(key, value) => {
        if !is_legal_key(&key) {
          return Err(CookieError(format!("Illegal key {key:?}")));
        }
        if key == name {
          found = Some(value);
        }
      }
    }
  }
  Ok(found)
}

/// `http.cookies._unquote`: strip one pair of double quotes and decode `\ooo` and `\x` escapes.
fn unquote(value: &str) -> String {
  if value.chars().count() < 2 || !value.starts_with('"') || !value.ends_with('"') {
    return value.to_string();
  }
  let inner: Vec<char> = value[1..value.len() - 1].chars().collect();
  let mut out = String::with_capacity(inner.len());
  let mut index = 0;
  while index < inner.len() {
    if inner[index] == '\\' && index + 1 < inner.len() {
      let octal: String = inner[index + 1..inner.len().min(index + 4)].iter().collect();
      let bytes = octal.as_bytes();
      if bytes.len() == 3 && (b'0'..=b'3').contains(&bytes[0]) && bytes[1..].iter().all(|b| (b'0'..=b'7').contains(b)) {
        out.push(
          char::from_u32(u32::from_str_radix(&octal, 8).expect("checked octal digits")).expect("a byte is a char"),
        );
        index += 4;
      } else {
        out.push(inner[index + 1]);
        index += 2;
      }
    } else {
      out.push(inner[index]);
      index += 1;
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  const NAME: &str = "charliebot_access_key";

  #[test]
  fn reads_the_cookie_among_others() {
    assert_eq!(cookie_value("a=1; charliebot_access_key=k-1; b=2", NAME), Ok(Some("k-1".to_string())));
    assert_eq!(cookie_value("charliebot_access_key=\"k\\0541\"", NAME), Ok(Some("k,1".to_string())));
    assert_eq!(cookie_value("a=1;b=2", NAME), Ok(None));
    assert_eq!(cookie_value("charliebot_access_key=old; charliebot_access_key=new", NAME), Ok(Some("new".to_string())));
  }

  #[test]
  fn invalid_headers_hide_the_cookie_like_simplecookie() {
    // A reserved attribute before any cookie discards the whole header.
    assert_eq!(cookie_value("Path=/; charliebot_access_key=k", NAME), Ok(None));
    // A value-less name that is not a flag discards the whole header.
    assert_eq!(cookie_value("charliebot_access_key=k; junk", NAME), Ok(None));
    // A flag attribute without a value is fine.
    assert_eq!(cookie_value("charliebot_access_key=k; Secure", NAME), Ok(Some("k".to_string())));
    // An illegal cookie name raises CookieError.
    assert!(cookie_value("a,b=1; charliebot_access_key=k", NAME).is_err());
    // A non-reserved $attribute after a cookie raises CookieError.
    assert!(cookie_value("charliebot_access_key=k; $Foo=1", NAME).is_err());
  }
}
