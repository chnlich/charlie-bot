//! Prints the minified text the server serves for each compressed module body, as one JSON object keyed by the loader's
//! name.
//!
//! The node suites read this object in server-artifact mode (`web/tests/read_static.js`), so a suite reads the text the
//! page serves rather than the source it was built from. A compressed name whose loader source is a stub is left out:
//! the page inlines the stub, and the module the stub loads stays a static file that the suites read from its source.
//! The build script decides which rows are module bodies.

use std::io::Write;

use charliebot_inline_js::SERVED_BODIES;

fn main() {
  let served: serde_json::Map<String, serde_json::Value> =
    SERVED_BODIES.iter().map(|(name, text)| (name.to_string(), serde_json::Value::String(text.to_string()))).collect();
  let json = serde_json::Value::Object(served).to_string();
  std::io::stdout().write_all(json.as_bytes()).expect("stdout is writable");
}
