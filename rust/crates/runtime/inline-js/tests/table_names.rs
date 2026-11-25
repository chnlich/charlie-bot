//! The table holds a row for each `.js` name a page template includes, and for the one name the server renders by name,
//! and for no other name.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use charliebot_inline_js::{COMPRESSED, PASS_THROUGH, ROWS};

/// The `.js` template the server renders by name, which no template includes.
const RENDERED_BY_SERVER: &[&str] = &["offline-shell-worker.js"];

/// Adds each `include '<name>.js'` that a template under `dir` holds.
fn included_names(dir: &Path, found: &mut BTreeSet<String>) {
  for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display())) {
    let path = entry.expect("readable directory entry").path();
    if path.is_dir() {
      included_names(&path, found);
    } else if matches!(path.extension().and_then(|ext| ext.to_str()), Some("html" | "js")) {
      let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
      for rest in text.split("include '").skip(1) {
        let name = rest.split('\'').next().unwrap_or_default();
        if name.ends_with(".js") {
          found.insert(name.to_string());
        }
      }
    }
  }
}

#[test]
fn the_table_holds_each_loader_served_name_and_no_other() {
  let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../web");
  let mut served = BTreeSet::new();
  for tree in ["templates", "features", "backends"] {
    included_names(&web.join(tree), &mut served);
  }
  served.extend(RENDERED_BY_SERVER.iter().map(|name| name.to_string()));

  let table: BTreeSet<String> = ROWS.iter().map(|row| row.name.to_string()).collect();
  let listed: BTreeSet<String> = COMPRESSED.iter().chain(PASS_THROUGH).map(|name| name.to_string()).collect();
  assert_eq!(table, listed, "the table holds the names src/names.rs lists");
  assert_eq!(table, served, "the table holds the names the page templates include");
}
