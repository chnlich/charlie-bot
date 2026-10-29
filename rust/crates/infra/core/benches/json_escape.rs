//! Micro-benchmark of the ASCII JSON string escaper before and after its `\uXXXX` escapes stopped going
//! through `format!`.
//!
//! Run with `cargo bench -p charliebot-core --bench json_escape`. Both escapers run on the same input in an
//! alternating loop inside one process and one profile, and each line prints the nanoseconds per call of
//! one case and side. Take the median of repeated runs per line.

use std::hint::black_box;
use std::time::Instant;

use charliebot_core::pyfmt::write_ascii_json_str;

/// The pre-change escaper, unchanged: every `\uXXXX` escape came from `format!`. It stays out of line so
/// both sides are plain calls; the library function is not inlined across the crate boundary either.
#[inline(never)]
fn old_write_ascii_json_str(out: &mut String, text: &str) {
  if text.bytes().all(|byte| (b' '..=b'~').contains(&byte) && byte != b'"' && byte != b'\\') {
    out.push('"');
    out.push_str(text);
    out.push('"');
    return;
  }
  out.push('"');
  for character in text.chars() {
    match character {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      '\u{8}' => out.push_str("\\b"),
      '\u{c}' => out.push_str("\\f"),
      c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
      c if (c as u32) <= 0x7e => out.push(c),
      c => {
        let mut units = [0u16; 2];
        for unit in c.encode_utf16(&mut units) {
          out.push_str(&format!("\\u{unit:04x}"));
        }
      }
    }
  }
  out.push('"');
}

/// A deterministic corpus of `chars` characters drawn from `alphabet`.
fn corpus(chars: usize, alphabet: &[char]) -> String {
  let mut state = 0x2545_f491_4f6c_dd1du64;
  let mut next = move || {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
  };
  (0..chars).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect()
}

fn main() {
  let cjk: Vec<char> = (0x4e00..0x4f00).filter_map(char::from_u32).chain(['\u{1f600}', '\u{2014}']).collect();
  let control: Vec<char> = (0u32..0x20).filter_map(char::from_u32).chain(['"', '\\', 'a']).collect();
  let mixed: Vec<char> = (0x20..0x7f)
    .filter_map(char::from_u32)
    .chain((0x4e00..0x4f00).filter_map(char::from_u32))
    .chain((0u32..0x20).filter_map(char::from_u32))
    .chain(['"', '\\', '\u{1f600}'])
    .collect();
  // The chat page's sidebar holds about 640 KB of JSON, mostly CJK text; every non-ASCII character and
  // every control character takes the escape the change rewrote.
  let cases = [
    ("sidebar-cjk-640k", corpus(400_000, &cjk), 10),
    ("control-64k", corpus(40_000, &control), 50),
    ("mixed-256k", corpus(160_000, &mixed), 20),
  ];
  for (name, text, iterations) in &cases {
    let text = black_box(text.as_str());
    let (mut old_time, mut new_time) = (0u128, 0u128);
    let mut sink = 0usize;
    for round in 0..*iterations {
      // Alternate the sides so a drifting clock charges both equally.
      let mut out = String::with_capacity(text.len() * 2);
      if round % 2 == 0 {
        let start = Instant::now();
        old_write_ascii_json_str(&mut out, text);
        old_time += start.elapsed().as_nanos();
      } else {
        let start = Instant::now();
        write_ascii_json_str(&mut out, text);
        new_time += start.elapsed().as_nanos();
      }
      sink += out.len();
    }
    black_box(sink);
    let (old_per_call, new_per_call) = (old_time / *iterations as u128, new_time / *iterations as u128);
    let speedup = old_per_call as f64 / new_per_call as f64;
    println!("{name}\told\t{old_per_call}\tnew\t{new_per_call}\tspeedup\t{speedup:.3}");
  }
}
