//! Query-parameter encoding: Python `urllib.parse.urlencode`'s wire form.

/// `urllib.parse.urlencode(pairs)` for an ordered mapping: `quote_plus` over each key and value,
/// `&`-joined in the mapping's order.
pub fn urlencode(params: &[(String, String)]) -> String {
  params.iter().map(|(key, value)| format!("{}={}", quote_plus(key), quote_plus(value))).collect::<Vec<_>>().join("&")
}

/// `urllib.parse.quote_plus` with the default `safe=''` turns a space into `+`. Every byte outside
/// ASCII letters, digits and `_.-~` becomes one uppercase `%XX` pair of its UTF-8 form.
fn quote_plus(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  for byte in text.bytes() {
    match byte {
      b' ' => out.push('+'),
      b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(byte as char),
      other => out.push_str(&format!("%{other:02X}")),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn keeps_the_unreserved_bytes_and_quotes_the_rest() {
    assert_eq!(quote_plus("plain-ascii_1.2~3"), "plain-ascii_1.2~3");
    assert_eq!(quote_plus("two words"), "two+words");
    assert_eq!(quote_plus("a/b"), "a%2Fb");
    assert_eq!(quote_plus("中文"), "%E4%B8%AD%E6%96%87");
  }

  #[test]
  fn joins_pairs_in_order() {
    let params = vec![("before".to_string(), "2".to_string()), ("limit".to_string(), "200".to_string())];
    assert_eq!(urlencode(&params), "before=2&limit=200");
  }
}
