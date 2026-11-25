//! The page loader's `.js` table: one row per loader-served name, built by `build.rs` into OUT_DIR.
//!
//! A row carries the sha256 of the source the table was built from, so the loader can refuse a source that changed
//! after the build. The compressed names carry their minified text; the pass-through names carry none.

mod names;

use sha2::{Digest, Sha256};

pub use names::{COMPRESSED, PASS_THROUGH};

/// One name's row. `minified` is the minified text, or `None` for a pass-through name.
pub struct Row {
  pub name: &'static str,
  pub sha256: &'static str,
  pub minified: Option<&'static str>,
}

include!(concat!(env!("OUT_DIR"), "/inline_js_table.rs"));

/// The row for `name`, if the table holds one.
pub fn row(name: &str) -> Option<&'static Row> {
  ROWS.iter().find(|row| row.name == name)
}

/// The lowercase hex sha256 of `bytes`, the form each row's `sha256` takes.
pub fn sha256_hex(bytes: &[u8]) -> String {
  Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}
