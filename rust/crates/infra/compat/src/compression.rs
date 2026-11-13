//! The request path's one-shot gzip deflators, and the zstd form of a body a memo serves.
//!
//! A response pays the deflate once per body it builds; a memo serves the repeats. The level-1
//! deflator covers the per-response bodies of ordinary size, and its byte count is one counter. A
//! per-response body too large for its short hash chain to find the long matches takes the level-4
//! form instead. The level-9 deflator covers a body a memo keeps and serves on every repeat, where
//! the smaller output reaches every repeat request and is worth a slower one-time deflate.
//!
//! A body a memo keeps also has a zstd form, which a client that accepts zstd takes instead: zstd
//! compresses a page's markup and inline script smaller than gzip does, and the memo pays its slower
//! compression once per body version.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::write::GzEncoder;

use crate::mimetypes_table::{
  APPLICATION_GZIP, APPLICATION_MSWORD, APPLICATION_PDF, APPLICATION_VND_MS_POWERPOINT, APPLICATION_X_7Z_COMPRESSED,
  APPLICATION_ZIP, FONT_WOFF, IMAGE_GIF, IMAGE_HEIC, IMAGE_JPEG, IMAGE_PNG, IMAGE_VND_MICROSOFT_ICON, IMAGE_WEBP,
};

static LEVEL_ONE_BYTES_DEFLATED: AtomicU64 = AtomicU64::new(0);

/// The smallest complete body worth deflating: `GZipMiddleware(minimum_size=1000)` of the transport, and the
/// smallest body a page compresses itself.
pub const MINIMUM_DEFLATE_BYTES: usize = 1000;

/// The server-sent event media type. The gzip skip list and the server's identity responder both exclude it, so the
/// name is spelled once here.
pub const TEXT_EVENT_STREAM: &str = "text/event-stream";

/// `_TRANSPORT_GZIP_SKIP_MEDIA_PREFIXES`: content types already entropy-coded, matched by prefix. A content type the
/// MIME table names is spelled there and read here.
pub const SKIP_MEDIA_PREFIXES: &[&str] = &[
  TEXT_EVENT_STREAM,
  IMAGE_GIF,
  IMAGE_HEIC,
  IMAGE_JPEG,
  IMAGE_PNG,
  IMAGE_WEBP,
  "image/x-icon",
  IMAGE_VND_MICROSOFT_ICON,
  "video/",
  "audio/",
  APPLICATION_PDF,
  APPLICATION_ZIP,
  "application/x-zip-compressed",
  APPLICATION_X_7Z_COMPRESSED,
  APPLICATION_GZIP,
  "application/x-gzip",
  "application/vnd.openxmlformats-officedocument",
  APPLICATION_VND_MS_POWERPOINT,
  APPLICATION_MSWORD,
  FONT_WOFF,
  "application/font-woff",
];

/// The level-1 gzip form of *data*; deterministic for equal input, with the gzip header's mtime
/// field zeroed.
pub fn gzip_level_one(data: &[u8]) -> Vec<u8> {
  LEVEL_ONE_BYTES_DEFLATED.fetch_add(data.len() as u64, Ordering::Relaxed);
  compress(data, 1)
}

/// The level-4 gzip form of a large body a response compresses afresh.
///
/// Level 1 keeps the fastest deflate, but its short hash chain misses the long matches of a large body built from
/// near-identical rows, so its output can run well past the level-9 form's. Level 4 recovers that size for a small
/// fraction of the level-9 cost.
pub fn gzip_level_four(data: &[u8]) -> Vec<u8> {
  compress(data, 4)
}

/// The level-9 gzip form of a body a memo serves on every repeat request.
///
/// The memo keeps the output, so one body pays the slowest level once and every repeat request reads the smaller
/// bytes. On the 7,160-session mock home the chat page body falls from 99,596 to 98,326 bytes (1.28%) for 2.2 ms
/// more of a 5.3 ms deflate, and the compact listing from 172,109 to 169,758 (1.37%).
pub fn gzip_memoized_body(data: &[u8]) -> Vec<u8> {
  compress(data, 9)
}

/// The zstd level of a body a memo serves on every repeat request.
///
/// The memo keeps the output, so the one-time compression buys the smaller bytes for every repeat request. Level 19 is
/// the slowest level: on the 7,160-session mock home the chat page body falls from 99,974 bytes (gzip level 9) to
/// 86,817 (13.2%) for about 120 ms of a version's first request.
pub const ZSTD_MEMOIZED_LEVEL: i32 = 19;

/// The level-19 zstd form of a body a memo serves on every repeat request.
pub fn zstd_memoized_body(data: &[u8]) -> Vec<u8> {
  zstd::encode_all(data, ZSTD_MEMOIZED_LEVEL).expect("compressing into a Vec cannot fail")
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

  #[test]
  fn the_zstd_form_decodes_to_its_body_and_is_smaller_than_the_gzip_form() {
    let body = b"<div class=\"page\"><script>const answer = 42;</script></div>\n".repeat(200);
    let zstd = zstd_memoized_body(&body);
    let decoded = zstd::decode_all(zstd.as_slice()).expect("the zstd form decodes");
    assert_eq!(decoded, body, "the zstd form decodes to the body it compressed");
    assert!(zstd.len() < gzip_memoized_body(&body).len(), "the memo's zstd form is smaller than its gzip form");
  }
}
