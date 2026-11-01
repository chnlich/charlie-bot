//! The request path's one-shot gzip deflators (`src/infra/compression.py`).
//!
//! A response pays the deflate once per body it builds; a memo serves the repeats. The level-1
//! deflator covers every body that is compressed per response, and its byte count is one counter.
//! The level-6 deflator covers a body a memo keeps and serves on every repeat, where the smaller
//! output is worth a slower one-time deflate.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::write::GzEncoder;

static LEVEL_ONE_BYTES_DEFLATED: AtomicU64 = AtomicU64::new(0);

/// The level-1 gzip form of *data*; deterministic for equal input, with the gzip header's mtime
/// field zeroed like Python's `gzip_level1` (`isal_zlib.compressobj(1, DEFLATED, 31)`).
pub fn gzip_level_one(data: &[u8]) -> Vec<u8> {
  LEVEL_ONE_BYTES_DEFLATED.fetch_add(data.len() as u64, Ordering::Relaxed);
  compress(data, 1)
}

/// The level-6 gzip form of a body a memo serves on every repeat request.
///
/// On the 7,160-session mock home a page body is 458 KB here and 700 KB at level 1; its first build
/// pays 14 ms instead of 6 ms. The memo keeps the output, so a repeat pays no deflate.
pub fn gzip_memoized_body(data: &[u8]) -> Vec<u8> {
  compress(data, 6)
}

fn compress(data: &[u8], level: u32) -> Vec<u8> {
  let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::new(level));
  encoder.write_all(data).expect("writing to a Vec cannot fail");
  encoder.finish().expect("finishing into a Vec cannot fail")
}

/// The request-path bytes fed to the level-1 deflator since server start.
pub fn level_one_bytes_deflated() -> u64 {
  LEVEL_ONE_BYTES_DEFLATED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn each_call_charges_its_input_bytes() {
    let body = b"synthetic body for the deflated byte count".repeat(50);
    let before = level_one_bytes_deflated();
    let compressed = gzip_level_one(&body);
    assert_eq!(level_one_bytes_deflated() - before, body.len() as u64);
    let before = level_one_bytes_deflated();
    let again = gzip_level_one(&body);
    assert_eq!(level_one_bytes_deflated() - before, body.len() as u64);
    assert_eq!(compressed, again, "equal input deflates to equal bytes");
  }
}
