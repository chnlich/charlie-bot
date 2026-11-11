//! Percent quoting as a safe set over the `percent-encoding` crate. Each caller names its safe bytes; the crate
//! does the byte loop and writes uppercase `%XX`. [`quote_plus`] and [`encode_pairs`] are the one form/query encoder
//! the CLI's calls and the server's routes share.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// The bytes that plain percent quoting encodes: every ASCII byte except the alphanumerics, `_.-~` and
/// `safe`. The crate also encodes every non-ASCII byte.
pub const fn quote_set(safe: &[u8]) -> AsciiSet {
  let mut set = NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~');
  let mut index = 0;
  while index < safe.len() {
    set = set.remove(safe[index]);
    index += 1;
  }
  set
}

/// The safe set of one form/query component: `_.-~` stays plain, and every other byte is quoted.
const QUERY_QUOTE_SET: &AsciiSet = &quote_set(&[]);

/// The quoted form of one form/query text component: a space becomes `+`, and every byte outside ASCII letters,
/// digits and `_.-~` becomes one uppercase `%XX` pair of its UTF-8 form.
pub fn quote_plus(text: &str) -> String {
  text.split(' ').map(|part| utf8_percent_encode(part, QUERY_QUOTE_SET).to_string()).collect::<Vec<_>>().join("+")
}

/// The wire form of an ordered mapping: `quote_plus` over each key and value, `&`-joined in the mapping's order.
pub fn encode_pairs(params: &[(String, String)]) -> String {
  params.iter().map(|(key, value)| format!("{}={}", quote_plus(key), quote_plus(value))).collect::<Vec<_>>().join("&")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn keeps_the_unreserved_bytes_and_quotes_the_rest() {
    assert_eq!(quote_plus("plain-ascii_1.2~3"), "plain-ascii_1.2~3");
    assert_eq!(quote_plus("two words"), "two+words");
    assert_eq!(quote_plus("a/b"), "a%2Fb");
    assert_eq!(quote_plus("a b/é~"), "a+b%2F%C3%A9~");
    assert_eq!(quote_plus("中文"), "%E4%B8%AD%E6%96%87");
  }

  #[test]
  fn joins_pairs_in_order() {
    let params = vec![("before".to_string(), "2".to_string()), ("limit".to_string(), "200".to_string())];
    assert_eq!(encode_pairs(&params), "before=2&limit=200");
  }
}
