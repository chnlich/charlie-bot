//! The request path's one-shot level-1 gzip deflator (`src/infra/compression.py`).
//!
//! Every gzip memo on the request path deflates through here, so the deflated byte count is one
//! counter. A body's first response pays the deflate once and its memo serves the repeats; that
//! count is the one the latency tests pin.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::write::GzEncoder;

static LEVEL_ONE_BYTES_DEFLATED: AtomicU64 = AtomicU64::new(0);

/// The level-1 gzip form of *data*; deterministic for equal input, with the gzip header's mtime
/// field zeroed like Python's `gzip_level1` (`isal_zlib.compressobj(1, DEFLATED, 31)`).
pub fn gzip_level_one(data: &[u8]) -> Vec<u8> {
  LEVEL_ONE_BYTES_DEFLATED.fetch_add(data.len() as u64, Ordering::Relaxed);
  let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::new(1));
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
