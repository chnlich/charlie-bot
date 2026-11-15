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
//!
//! A page's body is built per request, but its last substituted value ends near the front of the
//! document: the scripts and stylesheets after it are the same bytes in every body the page serves.
//! [`PageTail`] keeps the compressed forms of those bytes, so a body pays the deflate of its own head
//! and copies the tail's form.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compress, Compression as DeflateLevel, Crc, FlushCompress, Status};

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
/// The memo keeps the output, so the one-time compression buys the smaller bytes for every repeat request. Level 19
/// trades a slower one-time compression for a smaller body: on the 7,160-session mock home the chat page body falls
/// from 99,974 bytes (gzip level 9) to 86,817 (13.2%) for about 120 ms of a version's first request.
pub const ZSTD_MEMOIZED_LEVEL: i32 = 19;

/// The level-19 zstd form of a body a memo serves on every repeat request.
pub fn zstd_memoized_body(data: &[u8]) -> Vec<u8> {
  zstd::encode_all(data, ZSTD_MEMOIZED_LEVEL).expect("compressing into a Vec cannot fail")
}

/// The zstd level of a page head, the part of a body that changes with the request.
///
/// The head is deflated on every request that serves it, so its level pays per request. On the mock home's
/// 80,772-byte chat page head (version 2) level 19 takes 45.9 ms and saves 743 bytes over level 9, which takes
/// 1.1 ms; levels 12 and 15 already cost 11.0 and 18.5 ms for the smaller form.
const ZSTD_HEAD_LEVEL: i32 = 9;

/// The gzip level of a page head, the part of a body that a request builds.
///
/// A head deflates on every request that builds a body, so its level pays per request, while the bytes it saves reach
/// a client once per body. On the mock home's 80,772-byte chat page head (version 2) level 9 spends about 0.3 ms more
/// than level 6 for 87 fewer bytes, so the head takes level 6.
const GZIP_HEAD_LEVEL: u32 = 6;

/// The gzip level of a page tail: the bytes every body of a page version ends with.
///
/// The tail's deflate is built once per page version and appended to every body that version serves, so it takes the
/// smallest form: on the mock home's 316 KB chat page tail level 9 spends 2.5 ms once for 239 fewer bytes than level 6.
const GZIP_TAIL_LEVEL: u32 = 9;

/// The ten bytes of a gzip member's header: no name, comment or extra field, no modification time, the best-compression
/// flag and an unknown operating system.
const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff];

/// The compressed forms of a page's static tail: the bytes a page's bodies end with in common.
///
/// The tail is the same bytes in every body a page serves, so its deflate is built once and appended to the form of
/// the head that request built. A gzip member cannot be continued after its end: Chrome and curl stop at the first
/// member's end and drop the rest. The head's stream therefore ends with a full flush, which resets the deflate
/// history and ends the block, and the tail's deflate — built from a fresh deflator — decodes after it in the same
/// member. A zstd stream carries frames, and a decoder reads the frames in turn, so the tail travels as its own
/// frame. The boundary costs the matches that crossed it: on the mock home's chat page the gzip form grows from
/// 97,683 to 98,474 bytes and the zstd form from 84,552 to 87,212.
pub struct PageTail {
  tail: Vec<u8>,
  deflate: Vec<u8>,
  crc: Crc,
  zstd: Option<Vec<u8>>,
}

impl PageTail {
  /// The compressed forms of `tail`'s bytes, with the zstd form built at its first request.
  pub fn new(tail: &[u8]) -> PageTail {
    let mut crc = Crc::new();
    crc.update(tail);
    PageTail { tail: tail.to_vec(), deflate: deflate_of(tail, GZIP_TAIL_LEVEL), crc, zstd: None }
  }

  /// Whether this entry holds the forms of `tail`'s bytes.
  pub fn holds(&self, tail: &[u8]) -> bool {
    self.tail == tail
  }

  /// The tail's bytes.
  pub fn bytes(&self) -> &[u8] {
    &self.tail
  }

  /// The tail's length in bytes.
  pub fn len(&self) -> usize {
    self.tail.len()
  }

  /// Whether the tail holds no bytes.
  pub fn is_empty(&self) -> bool {
    self.tail.is_empty()
  }

  /// The gzip member carrying `head` and this tail.
  pub fn gzip(&self, head: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(GZIP_HEADER.len() + head.len() / 5 + self.deflate.len() + 8);
    out.extend_from_slice(&GZIP_HEADER);
    deflate_head_into(&mut out, head);
    out.extend_from_slice(&self.deflate);
    let mut crc = Crc::new();
    crc.update(head);
    crc.combine(&self.crc);
    out.extend_from_slice(&crc.sum().to_le_bytes());
    out.extend_from_slice(&((head.len() + self.tail.len()) as u32).to_le_bytes());
    out
  }

  /// The zstd stream carrying `head` in its first frame and this tail in its second.
  pub fn zstd(&mut self, head: &[u8]) -> Vec<u8> {
    let tail = self
      .zstd
      .get_or_insert_with(|| zstd::encode_all(self.tail.as_slice(), ZSTD_MEMOIZED_LEVEL).expect("a zstd frame builds"));
    let mut out = zstd::encode_all(head, ZSTD_HEAD_LEVEL).expect("a zstd frame builds");
    out.extend_from_slice(tail);
    out
  }
}

/// The raw deflate form of `data`, an ended stream a page tail can be appended after a full flush.
fn deflate_of(data: &[u8], level: u32) -> Vec<u8> {
  let mut encoder = DeflateEncoder::new(Vec::new(), DeflateLevel::new(level));
  encoder.write_all(data).expect("writing to a Vec cannot fail");
  encoder.finish().expect("finishing into a Vec cannot fail")
}

/// Append `head`'s deflate to `out`, ending it with a full flush so a tail deflates from a fresh history after it.
///
/// A full flush reports `Ok` while its output still fills the chunk, so the flush has completed only when a call
/// leaves room in the chunk: breaking on the first `Ok` cut the head's stream short at 8 KiB.
fn deflate_head_into(out: &mut Vec<u8>, head: &[u8]) {
  let mut compressor = Compress::new(DeflateLevel::new(GZIP_HEAD_LEVEL), false);
  let mut chunk = [0u8; 8192];
  let mut consumed = 0;
  loop {
    let flush = if consumed < head.len() { FlushCompress::None } else { FlushCompress::Full };
    let before_in = compressor.total_in();
    let before_out = compressor.total_out();
    let status = compressor.compress(&head[consumed..], &mut chunk, flush).expect("compressing into a Vec cannot fail");
    consumed += (compressor.total_in() - before_in) as usize;
    let produced = (compressor.total_out() - before_out) as usize;
    out.extend_from_slice(&chunk[..produced]);
    if status == Status::BufError || (flush == FlushCompress::Full && produced == chunk.len()) {
      continue;
    }
    if flush == FlushCompress::Full {
      break;
    }
  }
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
  use std::io::Read;

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

  /// The static tail every body of the synthetic page carries.
  const PAGE_TAIL: &str =
    "<script>const tail = 1;</script><style>.page { color: #333; }</style></article></body></html>";

  /// A head whose deflate runs past the output chunk, as a served page's head does.
  fn large_head() -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    (0..120_000)
      .map(|_| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 24) as u8
      })
      .collect()
  }

  #[test]
  fn a_page_tail_rides_after_a_head_in_one_gzip_member() {
    let tail = PageTail::new(PAGE_TAIL.as_bytes());
    for head in [b"<html><body><article data-session=\"one\">".to_vec(), large_head()] {
      let member = tail.gzip(&head);
      let mut decoded = Vec::new();
      flate2::read::GzDecoder::new(member.as_slice())
        .read_to_end(&mut decoded)
        .expect("a gzip member carrying a head and a tail decodes");
      assert_eq!(decoded, [head.as_slice(), PAGE_TAIL.as_bytes()].concat());
      assert_eq!(tail.gzip(&head), member, "the same head and tail build the same member");
    }
  }

  #[test]
  fn the_tail_form_does_not_depend_on_the_head_it_follows() {
    // Two heads of different lengths and bytes: the full flush ends the head's stream, so both members carry the
    // tail's own deflate and each decodes to its head plus the same tail.
    let tail = PageTail::new(PAGE_TAIL.as_bytes());
    let heads: [&[u8]; 2] =
      [b"<html><body>", b"<html><body><article data-session=\"another session with a longer name\">"];
    let deflate = deflate_of(PAGE_TAIL.as_bytes(), GZIP_TAIL_LEVEL);
    for head in heads {
      let member = tail.gzip(head);
      assert!(
        member.windows(deflate.len()).any(|window| window == deflate.as_slice()),
        "the member carries the tail's one deflate"
      );
      let mut decoded = String::new();
      flate2::read::GzDecoder::new(member.as_slice()).read_to_string(&mut decoded).expect("the member decodes");
      assert_eq!(decoded.as_bytes(), [head, PAGE_TAIL.as_bytes()].concat());
    }
  }

  #[test]
  fn a_page_tail_rides_after_a_head_in_the_next_zstd_frame() {
    let mut tail = PageTail::new(PAGE_TAIL.as_bytes());
    let head = b"<html><body><article data-session=\"one\">";
    let stream = tail.zstd(head);
    let decoded = zstd::decode_all(stream.as_slice()).expect("a zstd stream carrying two frames decodes");
    assert_eq!(decoded, [head.as_slice(), PAGE_TAIL.as_bytes()].concat());
  }
}
