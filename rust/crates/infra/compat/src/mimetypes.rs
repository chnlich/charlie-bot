//! Content-type guessing as CPython's `mimetypes.guess_type(path)[0]` answers it on this host.
//!
//! The database is CPython's built-in table ([`mimetypes_table`]) overlaid by the system `mime.types`
//! files CPython reads at `mimetypes.init()`, in its order. A later entry for an extension replaces an
//! earlier one. A guess applies the suffix map (`.tgz` -> `.tar.gz`), drops one case-sensitive encoding
//! suffix (`.gz`), then looks the remaining extension up lowercased.

use std::collections::HashMap;

use crate::mimetypes_table;

/// `mimetypes.knownfiles`, read in this order when present.
const KNOWN_FILES: &[&str] = &[
  "/etc/mime.types",
  "/etc/httpd/mime.types",
  "/etc/httpd/conf/mime.types",
  "/etc/apache/mime.types",
  "/etc/apache2/mime.types",
  "/usr/local/etc/httpd/conf/mime.types",
  "/usr/local/lib/netscape/mime.types",
  "/usr/local/etc/httpd/conf/mime.types",
  "/usr/local/etc/mime.types",
];

pub struct MimeTypes {
  types: HashMap<String, String>,
}

impl MimeTypes {
  /// The built-in table plus every known `mime.types` file present on this host.
  pub fn load() -> MimeTypes {
    let mut database = MimeTypes::builtin();
    for file in KNOWN_FILES {
      // CPython reads the files that `os.path.isfile` admits and skips the rest silently.
      if std::path::Path::new(file).is_file()
        && let Ok(bytes) = std::fs::read(file)
      {
        database.read_mime_types(&String::from_utf8_lossy(&bytes));
      }
    }
    database
  }

  /// The CPython built-in table alone, without the host's `mime.types` files.
  pub fn builtin() -> MimeTypes {
    MimeTypes {
      types: mimetypes_table::TYPES_MAP.iter().map(|(ext, kind)| (ext.to_string(), kind.to_string())).collect(),
    }
  }

  /// `MimeTypes.readfp`: each line names a type then its extensions; `#` starts a comment.
  fn read_mime_types(&mut self, text: &str) {
    for line in text.lines() {
      let words: Vec<&str> = line.split_whitespace().take_while(|word| !word.starts_with('#')).collect();
      if let Some((kind, suffixes)) = words.split_first() {
        for suffix in suffixes {
          self.types.insert(format!(".{suffix}"), kind.to_string());
        }
      }
    }
  }

  /// `mimetypes.guess_type(path)[0]`, or `None` when the extension is unknown.
  pub fn guess_type(&self, path: &str) -> Option<&str> {
    let (mut base, mut ext) = splitext(path);
    while let Some((_, expanded)) = mimetypes_table::SUFFIX_MAP.iter().find(|(suffix, _)| *suffix == ext.to_lowercase())
    {
      let joined = format!("{base}{expanded}");
      let (next_base, next_ext) = splitext(&joined);
      (base, ext) = (next_base.to_string(), next_ext.to_string());
    }
    if mimetypes_table::ENCODINGS_MAP.iter().any(|(suffix, _)| *suffix == ext) {
      let (_, inner_ext) = splitext(&base);
      ext = inner_ext.to_string();
    }
    self.types.get(&ext.to_lowercase()).map(String::as_str)
  }
}

/// `posixpath.splitext`: the extension is the last dot's suffix within the final component, ignoring
/// the component's leading dots.
fn splitext(path: &str) -> (String, String) {
  let sep_index = path.rfind('/').map_or(-1, |index| index as isize);
  if let Some(dot_index) = path.rfind('.')
    && dot_index as isize > sep_index
    && path[(sep_index + 1) as usize..dot_index].chars().any(|c| c != '.')
  {
    return (path[..dot_index].to_string(), path[dot_index..].to_string());
  }
  (path.to_string(), String::new())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn guesses_follow_the_rules() {
    let database = MimeTypes::builtin();
    let cases = [
      ("/a/app.js", Some("text/javascript")),
      ("/a/styles.CSS", Some("text/css")),
      ("/a/x.tar.gz", Some("application/x-tar")),
      ("/a/x.tgz", Some("application/x-tar")),
      ("/a/x.gz", None),
      ("/a/x.svgz", Some("image/svg+xml")),
      ("/a/.hidden", None),
      ("/a.d/noext", None),
      ("/a/icon.png", Some("image/png")),
    ];
    for (path, expected) in cases {
      assert_eq!(database.guess_type(path), expected, "{path}");
    }
  }

  #[test]
  fn a_mime_types_line_overrides_the_builtin_entry() {
    let mut database = MimeTypes::builtin();
    database.read_mime_types("# comment\napplication/x-test js mjs # trailing\n\n");
    assert_eq!(database.guess_type("/a/app.js"), Some("application/x-test"));
  }
}
