//! `urllib.parse.quote` as a safe set over the `percent-encoding` crate. Each caller names its `safe` bytes; the crate
//! does the byte loop and writes uppercase `%XX`.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};

/// The bytes `urllib.parse.quote(text, safe)` percent-encodes: every ASCII byte except the alphanumerics, `_.-~` and
/// `safe`. The crate also encodes every non-ASCII byte, as `quote` does.
pub const fn quote_set(safe: &[u8]) -> AsciiSet {
  let mut set = NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~');
  let mut index = 0;
  while index < safe.len() {
    set = set.remove(safe[index]);
    index += 1;
  }
  set
}
