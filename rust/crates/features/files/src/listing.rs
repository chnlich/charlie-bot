//! `_dir_listing_page` (`src/features/files/api.py`): the HTML page of one directory.
//!
//! The walk lists the directory and stats each child; a child that cannot be stat-ed is left out. The rows sort
//! directories first, then by lowercase name, and each row is escaped, quoted and appended in turn.

use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use charliebot_compat::file_urls::FILE_SERVER_MOUNT;
use charliebot_core::timefmt;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use time::OffsetDateTime;

use crate::human_size;

const FILENAME_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~');

/// The ways the walk ends without a page. Each one is an answer of the route.
#[derive(Debug)]
pub enum ListingError {
  /// The directory is gone (`FileNotFoundError`): 404.
  Missing,
  /// The directory cannot be listed (`PermissionError`): 403.
  PermissionDenied,
  /// Any other failure, such as an I/O error or a name the page cannot encode: 500.
  Failed { kind: &'static str, detail: String },
}

/// One child that `stat` could read.
struct Child {
  name: String,
  is_dir: bool,
  size: u64,
  modified: f64,
}

/// The listing page of `dir_path`, or `None` when `dir_path` is not a directory. `url_prefix` is the request path
/// that the rows link under.
pub fn dir_listing_page(dir_path: &Path, url_prefix: &str) -> Result<Option<String>, ListingError> {
  let entries = match fs::read_dir(dir_path).and_then(|entries| entries.collect::<io::Result<Vec<_>>>()) {
    Ok(entries) => entries,
    Err(error) => {
      return match error.kind() {
        ErrorKind::NotADirectory => Ok(None),
        ErrorKind::NotFound => Err(ListingError::Missing),
        ErrorKind::PermissionDenied => Err(ListingError::PermissionDenied),
        _ => Err(failed("OSError", format!("{}: {error}", dir_path.display()))),
      };
    }
  };
  let mut children = Vec::new();
  for entry in entries {
    // `except OSError: continue`: a child that cannot be stat-ed is skipped, not reported.
    let Ok(metadata) = fs::metadata(entry.path()) else { continue };
    let name =
      entry.file_name().into_string().map_err(|name| failed("UnicodeEncodeError", format!("{name:?} is not UTF-8")))?;
    children.push(Child {
      name,
      is_dir: metadata.is_dir(),
      size: metadata.len(),
      modified: timefmt::python_float_seconds(metadata.mtime(), metadata.mtime_nsec()),
    });
  }
  children.sort_by_cached_key(|child| (!child.is_dir, child.name.to_lowercase()));

  let mut rows = String::new();
  let base = url_prefix.trim_end_matches('/');
  if base != FILE_SERVER_MOUNT {
    let (parent, _) = base.rsplit_once('/').expect("a listed URL runs under the mount, so it holds a slash");
    rows.push_str(&format!(r#"<tr><td>📁</td><td><a href="{}">..</a></td><td></td><td></td></tr>"#, escape(parent)));
    rows.push('\n');
  }
  for child in &children {
    rows.push_str(&row(child, base)?);
    rows.push('\n');
  }
  let display_path =
    dir_path.to_str().ok_or_else(|| failed("UnicodeEncodeError", "the directory path is not UTF-8".into()))?;
  Ok(Some(page(&escape(display_path), &rows)))
}

/// One child's `<tr>`, without its newline.
fn row(child: &Child, base: &str) -> Result<String, ListingError> {
  let icon = if child.is_dir { "📁" } else { "📄" };
  let name = escape(&format!("{}{}", child.name, if child.is_dir { "/" } else { "" }));
  let href = escape(&format!("{base}/{}", quote(&child.name)));
  let size = if child.is_dir { String::new() } else { human_size::format_size(child.size) };
  let modified = modified_minute(child.modified)?;
  Ok(format!(
    r#"<tr><td>{icon}</td><td><a href="{href}">{name}</a></td><td style="text-align:right">{size}</td><td>{modified}</td></tr>"#
  ))
}

/// The page around the rows. Its markup is `_DIR_LISTING_TEMPLATE`, with its doubled braces read as single ones.
fn page(display_path: &str, rows: &str) -> String {
  format!(
    r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>Index of {display_path}</title>
<style>
  body {{ font-family: monospace; margin: 2em; }}
  table {{ border-collapse: collapse; }}
  td, th {{ padding: 4px 12px; text-align: left; }}
  a {{ text-decoration: none; color: #0366d6; }}
  a:hover {{ text-decoration: underline; }}
</style>
</head>
<body>
<h2>Index of {display_path}</h2>
<table>
<tr><th></th><th>Name</th><th>Size</th><th>Modified</th></tr>
{rows}
</table>
</body>
</html>"#
  )
}

/// `datetime.fromtimestamp(seconds, tz=UTC).strftime("%Y-%m-%d %H:%M")`. CPython rounds the fraction to whole
/// microseconds half to even, and a rounding carry of 1e6 microseconds moves the whole seconds on by one.
fn modified_minute(seconds: f64) -> Result<String, ListingError> {
  let micros = (seconds.fract() * 1e6).round_ties_even();
  let whole = if micros >= 1e6 {
    seconds.trunc() + 1.0
  } else if micros < 0.0 {
    seconds.trunc() - 1.0
  } else {
    seconds.trunc()
  };
  let moment =
    OffsetDateTime::from_unix_timestamp(whole as i64).map_err(|error| failed("OverflowError", error.to_string()))?;
  Ok(format!(
    "{:04}-{:02}-{:02} {:02}:{:02}",
    moment.year(),
    u8::from(moment.month()),
    moment.day(),
    moment.hour(),
    moment.minute()
  ))
}

/// `html.escape(text)` with `quote=True`: the five characters that end an attribute value or a text node.
fn escape(text: &str) -> String {
  html_escape::encode_quoted_attribute(text).into_owned()
}

/// `urllib.parse.quote(name, safe='')`: every byte but ASCII letters, digits and `_.-~` becomes `%XX`.
fn quote(name: &str) -> String {
  utf8_percent_encode(name, FILENAME_SET).to_string()
}

fn failed(kind: &'static str, detail: String) -> ListingError {
  ListingError::Failed { kind, detail }
}

#[cfg(test)]
mod tests {
  use std::time::{Duration, UNIX_EPOCH};

  use super::*;

  /// 2026-01-01 12:00:00 UTC.
  const NOON: u64 = 1_767_268_800;

  fn listing(dir: &Path, url_prefix: &str) -> String {
    dir_listing_page(dir, url_prefix).expect("the directory lists").expect("the path is a directory")
  }

  fn set_mtime(path: &Path, seconds: u64, nanos: u32) {
    let time = UNIX_EPOCH + Duration::new(seconds, nanos);
    fs::File::options().write(true).open(path).expect("the file opens").set_modified(time).expect("mtime sets");
  }

  #[test]
  fn directories_come_first_then_names_in_lowercase_order() {
    let dir = tempfile::tempdir().expect("a temp dir");
    for name in ["b.txt", "A.txt", "a b.txt"] {
      fs::write(dir.path().join(name), "").expect("the file writes");
    }
    fs::create_dir(dir.path().join("zdir")).expect("the dir creates");
    fs::create_dir(dir.path().join("Adir")).expect("the dir creates");
    let page = listing(dir.path(), "/absolute_filepath");
    let order: Vec<usize> = ["Adir/", "zdir/", "a b.txt", "A.txt", "b.txt"]
      .iter()
      .map(|name| page.find(&format!(">{name}</a>")).unwrap_or_else(|| panic!("{name} is listed")))
      .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "rows out of order: {order:?}");
  }

  #[test]
  fn names_are_escaped_and_hrefs_are_quoted_under_the_request_path() {
    let dir = tempfile::tempdir().expect("a temp dir");
    fs::write(dir.path().join("<i>&\"x' é.txt"), "").expect("the file writes");
    let page = listing(dir.path(), "/absolute_filepath/tmp/a b/");
    assert!(
      page.contains(
        r#"<a href="/absolute_filepath/tmp/a b/%3Ci%3E%26%22x%27%20%C3%A9.txt">&lt;i&gt;&amp;&quot;x&#x27; é.txt</a>"#
      ),
      "{page}"
    );
  }

  #[test]
  fn a_file_row_shows_its_size_and_utc_minute_and_a_directory_row_shows_neither() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("notes.txt");
    fs::write(&file, vec![b'x'; 1536]).expect("the file writes");
    set_mtime(&file, NOON, 0);
    fs::create_dir(dir.path().join("sub")).expect("the dir creates");
    let page = listing(dir.path(), "/absolute_filepath/tmp");
    assert!(
      page.contains(
        r#"<tr><td>📄</td><td><a href="/absolute_filepath/tmp/notes.txt">notes.txt</a></td><td style="text-align:right">1.5 KB</td><td>2026-01-01 12:00</td></tr>"#
      ),
      "{page}"
    );
    assert!(
      page.contains(
        r#"<tr><td>📁</td><td><a href="/absolute_filepath/tmp/sub">sub/</a></td><td style="text-align:right"></td><td>"#
      ),
      "{page}"
    );
  }

  #[test]
  fn a_fraction_that_rounds_to_a_whole_second_carries_into_the_minute() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("late.txt");
    fs::write(&file, "").expect("the file writes");
    set_mtime(&file, NOON + 59, 999_999_900);
    assert!(listing(dir.path(), "/absolute_filepath").contains("<td>2026-01-01 12:01</td>"));
  }

  #[test]
  fn the_parent_row_is_absent_at_the_mount_root_and_links_one_segment_up_below_it() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let at_root = listing(dir.path(), "/absolute_filepath/");
    assert!(!at_root.contains(">..</a>"), "{at_root}");
    let below = listing(dir.path(), "/absolute_filepath/x/y/");
    assert!(below.contains(r#"<tr><td>📁</td><td><a href="/absolute_filepath/x">..</a></td><td></td><td></td></tr>"#));
  }

  #[test]
  fn a_path_that_is_not_a_directory_is_none() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("plain.txt");
    fs::write(&file, "").expect("the file writes");
    assert!(dir_listing_page(&file, "/absolute_filepath/plain.txt").expect("no error").is_none());
  }

  #[test]
  fn html_and_file_url_escaping_match_python_bytes() {
    assert_eq!(escape("&<>\"'/"), "&amp;&lt;&gt;&quot;&#x27;/");
    assert_eq!(quote("a b/#?é~"), "a%20b%2F%23%3F%C3%A9~");
  }

  #[test]
  fn a_missing_directory_is_missing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let gone = dir.path().join("gone");
    assert!(matches!(dir_listing_page(&gone, "/absolute_filepath/gone"), Err(ListingError::Missing)));
  }

  #[test]
  fn an_unlistable_directory_is_permission_denied() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("a temp dir");
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).expect("the dir creates");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("the mode sets");
    let outcome = dir_listing_page(&locked, "/absolute_filepath/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("the mode restores");
    assert!(matches!(outcome, Err(ListingError::PermissionDenied)));
  }
}
