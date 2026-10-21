//! The host-file links accepted by `src/runtime/file_urls.py`.

use std::net::Ipv6Addr;
use std::path::PathBuf;

/// The first file-server mount, without its trailing slash.
pub const FILE_SERVER_MOUNT: &str = "/absolute_filepath";

/// The host file a link names, or `None` when it does not name a file on this host.
///
/// File-server URLs use the absolute path after the mount, whatever scheme or host precedes it. A bare absolute path
/// names itself only when it has no scheme, network location, query or fragment. Malformed bracketed authorities
/// return the error `urllib.parse.urlsplit` raises.
pub fn host_file(link: Option<&str>) -> Result<Option<PathBuf>, String> {
  let Some(link) = link.filter(|link| !link.is_empty()) else {
    return Ok(None);
  };
  let cleaned: String =
    link.trim_start_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
  let (before_fragment, fragment) = cleaned.split_once('#').unwrap_or((cleaned.as_str(), ""));
  let (head, query) = before_fragment.split_once('?').unwrap_or((before_fragment, ""));
  let scheme = scheme_rest(head);
  let after_scheme = scheme.unwrap_or(head);
  let (netloc, encoded_path) = match after_scheme.strip_prefix("//") {
    Some(authority) => authority.find('/').map_or((authority, ""), |index| authority.split_at(index)),
    None => ("", after_scheme),
  };
  validate_bracketed_netloc(netloc)?;
  let path = percent_decode_lossy(encoded_path);
  if path.starts_with(&format!("{FILE_SERVER_MOUNT}/")) {
    return Ok(Some(PathBuf::from(&path[FILE_SERVER_MOUNT.len()..])));
  }
  if scheme.is_none() && netloc.is_empty() && query.is_empty() && fragment.is_empty() && path.starts_with('/') {
    return Ok(Some(PathBuf::from(path)));
  }
  Ok(None)
}

/// The text after `:` when the input begins with a valid URL scheme.
fn scheme_rest(link: &str) -> Option<&str> {
  let (scheme, rest) = link.split_once(':')?;
  let mut chars = scheme.chars();
  if !chars.next().is_some_and(|first| first.is_ascii_alphabetic())
    || !chars.all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '.' | '-'))
  {
    return None;
  }
  Some(rest)
}

/// Python's URL splitter validates square-bracketed hosts while splitting the authority.
fn validate_bracketed_netloc(netloc: &str) -> Result<(), String> {
  let has_open_bracket = netloc.contains('[');
  let has_close_bracket = netloc.contains(']');
  if !has_open_bracket && !has_close_bracket {
    return Ok(());
  }
  if has_open_bracket != has_close_bracket {
    return Err("Invalid IPv6 URL".to_string());
  }

  let host_and_port = netloc.rsplit('@').next().unwrap_or(netloc);
  if let Some(after_open) = host_and_port.strip_prefix('[') {
    let Some((hostname, after_close)) = after_open.split_once(']') else {
      return Err("Invalid IPv6 URL".to_string());
    };
    if (!after_close.is_empty() && !after_close.starts_with(':')) || !valid_bracketed_host(hostname) {
      return Err("Invalid IPv6 URL".to_string());
    }
  } else {
    let hostname = host_and_port.split_once(':').map_or(host_and_port, |(host, _)| host);
    if !valid_bracketed_host(hostname) {
      return Err("Invalid IPv6 URL".to_string());
    }
  }
  Ok(())
}

/// An IPv6 or IPvFuture hostname allowed between URL authority brackets.
fn valid_bracketed_host(hostname: &str) -> bool {
  if let Some(version) = hostname.strip_prefix('v').or_else(|| hostname.strip_prefix('V')) {
    return version.split_once('.').is_some_and(|(hex, address)| {
      !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) && !address.is_empty()
    });
  }
  let address = hostname.split_once('%').map_or(hostname, |(address, zone)| if zone.is_empty() { "" } else { address });
  !address.is_empty() && address.parse::<Ipv6Addr>().is_ok()
}

/// Decode `%XX` bytes and replace invalid UTF-8 as `urllib.parse.unquote` does.
fn percent_decode_lossy(value: &str) -> String {
  let bytes = value.as_bytes();
  let mut decoded = Vec::with_capacity(bytes.len());
  let mut index = 0;
  while index < bytes.len() {
    if bytes[index] == b'%'
      && index + 2 < bytes.len()
      && let (Some(high), Some(low)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
    {
      decoded.push(high * 16 + low);
      index += 3;
      continue;
    }
    decoded.push(bytes[index]);
    index += 1;
  }
  String::from_utf8_lossy(&decoded).into_owned()
}

/// One ASCII hexadecimal digit's value.
fn hex_value(byte: u8) -> Option<u8> {
  match byte {
    b'0'..=b'9' => Some(byte - b'0'),
    b'a'..=b'f' => Some(byte - b'a' + 10),
    b'A'..=b'F' => Some(byte - b'A' + 10),
    _ => None,
  }
}

/// The form of a file link the scanner recognized (`FileLinkForm`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileLinkForm {
  /// A file-server URL: the mount, then an absolute path (`FileLinkForm.CANONICAL`).
  Canonical,
  /// A URL under the retired `/files/` or `/file/` prefix (`FileLinkForm.RETIRED_PREFIX`).
  RetiredPrefix,
  /// A Markdown target that names a path on this host (`FileLinkForm.LOCAL_PATH`).
  LocalPath,
}

/// The paths that resolve the links of one message: this server's port, the CharlieBot home, the session directory and
/// the user's home (`ScanContext`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanContext {
  pub server_port: u16,
  pub charliebot_home: PathBuf,
  pub session_dir: PathBuf,
  pub user_home: PathBuf,
}

/// One recognized file link and the scanner's liveness and correction result (`FileLink`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLink {
  pub link: String,
  pub form: FileLinkForm,
  pub path: Option<PathBuf>,
  pub live: Option<bool>,
  pub correct_link: Option<String>,
}

impl FileLink {
  /// Whether this finding names a link the reader cannot open (`FileLink.dead`).
  pub fn dead(&self) -> bool {
    self.live == Some(false)
  }
}

/// Finds the file links in `text` and classifies each one's form and liveness (`scan_file_links`).
pub fn scan_file_links(_text: &str, _context: Option<&ScanContext>) -> std::io::Result<Vec<FileLink>> {
  Ok(Vec::new())
}

/// The HTTP 422 detail for the dead findings in `findings` (`dead_links_detail`).
pub fn dead_links_detail(_findings: &[FileLink]) -> String {
  String::new()
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::{Path, PathBuf};

  use super::FileLinkForm::{LocalPath, RetiredPrefix};
  use super::*;

  #[test]
  fn host_file_paths_follow_python_url_splitting_and_unquoting() {
    assert_eq!(host_file(Some("/tmp/page.html")).unwrap(), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(host_file(Some(" /tmp/page.html")).unwrap(), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(host_file(Some("/tmp/page.html?")).unwrap(), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(host_file(Some("/tmp/page.html#")).unwrap(), Some(PathBuf::from("/tmp/page.html")));
    assert_eq!(
      host_file(Some("https://example.test/absolute_filepath/tmp%20page.html")).unwrap(),
      Some(PathBuf::from("/tmp page.html"))
    );
    assert_eq!(host_file(Some("/?session=synthetic")).unwrap(), None);
    assert_eq!(host_file(Some("https://github.com/org/repo/pull/1")).unwrap(), None);
    assert_eq!(host_file(Some("relative/page.html")).unwrap(), None);
    assert_eq!(host_file(None).unwrap(), None);
    assert_eq!(host_file(Some("")).unwrap(), None);
  }

  #[test]
  fn malformed_bracketed_authorities_fail_and_ipv6_hosts_pass() {
    for link in ["https://[not-ipv6]/page.html", "https://[::1]suffix/page.html", "https://host]/page.html"] {
      assert_eq!(host_file(Some(link)).unwrap_err(), "Invalid IPv6 URL", "link {link}");
    }
    assert_eq!(host_file(Some("https://[::1]/page.html")).unwrap(), None);
    assert_eq!(host_file(Some("https://[v1.example]/page.html")).unwrap(), None);
  }

  /// A scan context under `root`, with its home, session directory and user home created.
  fn synthetic_context(root: &Path) -> ScanContext {
    let context = ScanContext {
      server_port: 18498,
      charliebot_home: root.join("charliebot-home"),
      session_dir: root.join("sessions").join("synthetic-session"),
      user_home: root.join("user-home"),
    };
    for dir in [&context.charliebot_home, &context.session_dir, &context.user_home] {
      fs::create_dir_all(dir).unwrap();
    }
    context
  }

  /// Writes a one-line page at `path`, creating its parent directories.
  fn write_page(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "page").unwrap();
  }

  #[test]
  fn the_scanner_classifies_retired_and_markdown_local_targets() {
    let root = tempfile::tempdir().unwrap();
    let context = synthetic_context(root.path());
    let session_artifact = context.session_dir.join("artifacts/plan.html");
    let retired_session_file = context.charliebot_home.join("sessions/synthetic-session/plan.html");
    let retired_home_file = context.charliebot_home.join("home/synthetic-user/source.rs");
    for page in [&session_artifact, &retired_session_file, &retired_home_file] {
      write_page(page);
    }
    let session_url = "https://charliebot.example:18498/files/sessions/synthetic-session/plan.html";
    let home_url = "https://charliebot.example:18498/files/home/synthetic-user/source.rs";
    let text = format!(
      "{session_url} {home_url} [plan](artifacts/plan.html) [script](~/scripts/x.sh) [line](/home/u/x.rs:258)\n\
       [artifact]: artifacts/plan.html"
    );

    let findings = scan_file_links(&text, Some(&context)).unwrap();

    assert_eq!(
      findings.iter().map(|finding| finding.form).collect::<Vec<_>>(),
      [RetiredPrefix, RetiredPrefix, LocalPath, LocalPath, LocalPath, LocalPath]
    );
    assert_eq!(
      findings.iter().map(|finding| finding.link.as_str()).collect::<Vec<_>>(),
      [session_url, home_url, "artifacts/plan.html", "~/scripts/x.sh", "/home/u/x.rs:258", "artifacts/plan.html"]
    );
    assert!(findings.iter().all(FileLink::dead));
    assert_eq!(
      findings.iter().map(|finding| finding.correct_link.clone()).collect::<Vec<_>>(),
      [
        Some(format!("https://charliebot.example:18498{FILE_SERVER_MOUNT}{}", retired_session_file.display())),
        Some(format!("https://charliebot.example:18498{FILE_SERVER_MOUNT}{}", retired_home_file.display())),
        Some(format!("{FILE_SERVER_MOUNT}{}", session_artifact.display())),
        Some(format!("{FILE_SERVER_MOUNT}{}", context.user_home.join("scripts/x.sh").display())),
        Some(format!("{FILE_SERVER_MOUNT}/home/u/x.rs:258")),
        Some(format!("{FILE_SERVER_MOUNT}{}", session_artifact.display())),
      ]
    );
  }

  #[test]
  fn the_scanner_checks_local_canonical_paths_and_ignores_non_file_links() {
    let root = tempfile::tempdir().unwrap();
    let context = synthetic_context(root.path());
    let live_file = root.path().join("live.html");
    write_page(&live_file);
    let live_dir = root.path().join("directory");
    fs::create_dir(&live_dir).unwrap();
    let missing_file = root.path().join("missing.html");
    let served = |path: &Path| format!("http://localhost:18498{FILE_SERVER_MOUNT}{}", path.display());
    let (live_url, directory_url, missing_url) = (served(&live_file), served(&live_dir), served(&missing_file));
    let other_url = "https://other.example/absolute_filepath/tmp/not-on-this-host";
    let text = format!(
      "{live_url} {directory_url} {live_url}. {live_url}, {live_url}。 {missing_url} \
       [session](/?session=synthetic-id) https://code.example/o/r/pull/1/files \
       https://other.example/files/x https://charliebot.example:18499/files/x /diff?x /files/x {other_url}"
    );

    let findings = scan_file_links(&text, Some(&context)).unwrap();

    assert_eq!(
      findings.iter().map(|finding| finding.link.as_str()).collect::<Vec<_>>(),
      [
        live_url.as_str(),
        directory_url.as_str(),
        live_url.as_str(),
        live_url.as_str(),
        live_url.as_str(),
        missing_url.as_str(),
        other_url
      ]
    );
    assert_eq!(
      findings.iter().map(|finding| finding.live).collect::<Vec<_>>(),
      [Some(true), Some(true), Some(true), Some(true), Some(true), Some(false), None]
    );
    assert_eq!(
      findings.iter().map(FileLink::dead).collect::<Vec<_>>(),
      [false, false, false, false, false, true, false]
    );
    assert_eq!(findings[5].path, Some(missing_file));
  }
}
