//! Prints the minified text the server serves for each compressed module body, as one JSON object keyed by the loader's
//! name.
//!
//! The node suites read this object in server-artifact mode (`web/tests/read_static.js`), so a suite reads the text the
//! page serves rather than the source it was built from. A compressed name whose loader source is a stub is left out:
//! the page inlines the stub, and the module the stub loads stays a static file that the suites read from its source.
//! A row is a module body when its hash is the hash of the static file of the same name.

use std::io::Write;
use std::path::Path;
use std::{env, fs};

use charliebot_inline_js::{ROWS, sha256_hex};

fn main() {
  // A row's name is the loader's name, relative to web/static, as the template include spells it (`js/chat/shared.js`).
  let static_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../web/static");
  let mut served = serde_json::Map::new();
  for row in ROWS {
    let Some(text) = row.minified else { continue };
    let module = static_dir.join(row.name);
    if !module.is_file() {
      continue;
    }
    let bytes = fs::read(&module).unwrap_or_else(|error| panic!("{}: {error}", module.display()));
    if sha256_hex(&bytes) != row.sha256 {
      continue;
    }
    served.insert(row.name.to_string(), serde_json::Value::String(text.to_string()));
  }
  let json = serde_json::Value::Object(served).to_string();
  std::io::stdout().write_all(json.as_bytes()).expect("stdout is writable");
}
