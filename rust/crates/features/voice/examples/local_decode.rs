//! The differential's Rust side: loads the speech models once, then decodes each raw PCM file the arguments name.
//!
//! Usage: `local_decode <cache-dir> <pcm-file>...`. It prints one JSON line per file: the file, its decode windows
//! as `[start, end, left, right]` in samples, and the transcript.

use std::path::Path;

use anyhow::Context;
use charliebot_voice::differential::OfflineDecoder;
use serde_json::json;

fn main() -> anyhow::Result<()> {
  let mut args = std::env::args().skip(1);
  let cache_dir = args.next().context("usage: local_decode <cache-dir> <pcm-file>...")?;
  let decoder = OfflineDecoder::load(Path::new(&cache_dir))?;
  for pcm_path in args {
    let pcm = std::fs::read(&pcm_path).with_context(|| format!("read {pcm_path}"))?;
    let decoded = decoder.decode(&pcm)?;
    println!("{}", json!({ "pcm": pcm_path, "windows": decoded.windows, "text": decoded.text }));
  }
  Ok(())
}
