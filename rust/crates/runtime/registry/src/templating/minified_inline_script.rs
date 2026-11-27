//! The minified form of a page's inline script, from the build-time table of `charliebot-inline-js`.
//!
//! `USE_MINIFIED_INLINE_JS` turns the form on. Unset or `1` serves each compressed name's minified text, and a
//! pass-through name's source with its comments removed. `0` or `false` (any case) serves every name's source with its
//! comments removed, the form the loader served before the table. A name the table lacks, or a source whose sha256
//! differs from its row, fails the render and names the template, because the table holds no text for that source.

use charliebot_inline_js::{row, sha256_hex};
use minijinja::{Error, ErrorKind};

use crate::inlined_script;

/// The environment variable that turns the minified form on or off.
pub const USE_MINIFIED_INLINE_JS: &str = "USE_MINIFIED_INLINE_JS";

/// Whether the switch leaves the minified form on: unset means on, and `0` or `false` (any case) means off.
pub fn enabled_from_env() -> bool {
  enabled_from_value(std::env::var(USE_MINIFIED_INLINE_JS))
}

fn enabled_from_value(value: Result<String, std::env::VarError>) -> bool {
  match value {
    Ok(value) => {
      let value = value.trim();
      value != "0" && !value.eq_ignore_ascii_case("false")
    }
    Err(std::env::VarError::NotPresent | std::env::VarError::NotUnicode(_)) => true,
  }
}

/// The text the loader serves for the `.js` template `name` when the form is on, given its source `source`.
pub fn served(name: &str, source: &str) -> Result<String, Error> {
  let Some(row) = row(name) else {
    return Err(Error::new(
      ErrorKind::InvalidOperation,
      format!("{name} has no row in the minified inline script table; rebuild charliebot-inline-js"),
    ));
  };
  if sha256_hex(source.as_bytes()) != row.sha256 {
    return Err(Error::new(
      ErrorKind::InvalidOperation,
      format!("{name} changed after the minified inline script table was built; rebuild charliebot-inline-js"),
    ));
  }
  Ok(match row.minified {
    Some(minified) => minified.to_string(),
    None => inlined_script::without_comments(source),
  })
}

#[cfg(test)]
mod tests {
  use std::env::VarError;
  use std::fs;
  use std::path::Path;

  use charliebot_inline_js::row;

  use super::{enabled_from_value, served};
  use crate::inlined_script;

  /// The committed source of a `.js` name under web/static.
  fn static_source(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../web/static").join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
  }

  #[test]
  fn the_switch_is_on_unless_it_reads_zero_or_false() {
    assert!(enabled_from_value(Err(VarError::NotPresent)));
    assert!(enabled_from_value(Ok("1".to_string())));
    assert!(!enabled_from_value(Ok("0".to_string())));
    assert!(!enabled_from_value(Ok(" FALSE ".to_string())));
  }

  #[test]
  fn a_compressed_name_serves_its_table_text_and_a_pass_through_name_its_stripped_source() {
    let compressed = static_source("js/config.js");
    let table_text = row("js/config.js").and_then(|row| row.minified).expect("config.js is a compressed row");
    assert_eq!(served("js/config.js", &compressed).unwrap(), table_text);

    let passed = static_source("js/offline-shell-worker.js");
    assert_eq!(served("js/offline-shell-worker.js", &passed).unwrap(), inlined_script::without_comments(&passed));
  }

  #[test]
  fn a_name_without_a_row_or_with_a_changed_source_fails_and_names_the_template() {
    let missing = served("js/no-such-template.js", "let x = 1;").unwrap_err();
    assert!(missing.to_string().contains("js/no-such-template.js"), "{missing}");

    let changed = served("js/config.js", "const changed = 1;").unwrap_err();
    assert!(changed.to_string().contains("js/config.js"), "{changed}");
  }
}
