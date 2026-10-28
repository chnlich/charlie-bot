//! The host-file links accepted by `src/runtime/file_urls.py`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::net::Ipv6Addr;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use charliebot_core::paths;
use percent_encoding::{AsciiSet, percent_decode_str, percent_encode};
use regex::Regex;

use crate::urlquote::quote_set;

/// The first file-server mount, without its trailing slash.
pub const FILE_SERVER_MOUNT: &str = "/absolute_filepath";

/// The retired file-server prefixes, without their trailing slashes (`RETIRED_FILE_SERVER_PREFIXES`).
const RETIRED_PREFIXES: [&str; 2] = ["/files", "/file"];

/// Python's `str.isspace` characters, as the inside of a character class where its `re` matches `\s`.
const PY_SPACE: &str =
  concat!(r"\t\n\x0B\x0C\r\x1C-\x1F \x85\x{A0}\x{1680}\x{2000}-\x{200A}", r"\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}",);

/// Any URL reply text can carry: scheme, authority, optional port, path, query string and fragment (`_URL_RE`).
pub static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(&format!(
    r"[a-zA-Z][a-zA-Z0-9+.\-]*://(?:\[[^\]{PY_SPACE}]+\]|[^/:(){PY_SPACE}]+)(?::(?P<port>\d+))?(?P<path>/[^?#(){PY_SPACE}]*)?(?:\?[^#{PY_SPACE}]*)?(?:#[^{PY_SPACE}]*)?"
  ))
  .expect("valid pattern")
});

/// A Markdown inline link's target, `<...>` or bare (`_MARKDOWN_INLINE_TARGET_RE`).
static MARKDOWN_INLINE_TARGET_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(&format!(r"\[[^\]\n]*\]\([{PY_SPACE}]*(?P<target><[^>\n]*>|[^){PY_SPACE}]+)")).expect("valid pattern")
});

/// A Markdown reference definition's target, `<...>` or bare, at the start of a line (`_MARKDOWN_REFERENCE_TARGET_RE`).
static MARKDOWN_REFERENCE_TARGET_RE: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(&format!(r"(?m)^[ \t]{{0,3}}\[[^\]\n]+\]:[ \t]*(?P<target><[^>\n]*>|[^{PY_SPACE}]+)"))
    .expect("valid pattern")
});

/// The decimal digit zero code points in CPython 3.14.7's Unicode 16.0.0 data.
const DECIMAL_DIGIT_ZEROES: &[u32] = &[
  0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50, 0xED0, 0xF20,
  0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0,
  0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x10D40, 0x11066, 0x110F0, 0x11136, 0x111D0,
  0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x116D0, 0x116DA, 0x11730, 0x118E0, 0x11950, 0x11BF0, 0x11C50, 0x11D50,
  0x11DA0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50, 0x16D70, 0x1CCF0, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6,
  0x1E140, 0x1E2F0, 0x1E4F0, 0x1E5F1, 0x1E950, 0x1FBF0,
];

/// The schemes `urllib.parse` gives a network location (`uses_netloc`), without the empty scheme.
const USES_NETLOC: [&str; 26] = [
  "file",
  "ftp",
  "git",
  "git+ssh",
  "gopher",
  "http",
  "https",
  "imap",
  "itms-services",
  "mms",
  "nfs",
  "nntp",
  "prospero",
  "rsync",
  "rtsp",
  "rtsps",
  "rtspu",
  "sftp",
  "shttp",
  "snews",
  "svn",
  "svn+ssh",
  "telnet",
  "wais",
  "ws",
  "wss",
];

/// The host file a link names, or `None` when it does not name a file on this host.
///
/// File-server URLs use the absolute path after the mount, whatever scheme or host precedes it. A bare absolute path
/// names itself only when it has no scheme, network location, query or fragment. Malformed bracketed authorities
/// return the error `urllib.parse.urlsplit` raises.
pub fn host_file(link: Option<&str>) -> Result<Option<PathBuf>, String> {
  let Some(link) = link.filter(|link| !link.is_empty()) else {
    return Ok(None);
  };
  let parts = split_url(link)?;
  let path = percent_decode_lossy(&parts.path);
  if path.starts_with(&format!("{FILE_SERVER_MOUNT}/")) {
    return Ok(Some(PathBuf::from(&path[FILE_SERVER_MOUNT.len()..])));
  }
  if parts.scheme.is_empty()
    && parts.netloc.is_empty()
    && parts.query.is_empty()
    && parts.fragment.is_empty()
    && path.starts_with('/')
  {
    return Ok(Some(PathBuf::from(path)));
  }
  Ok(None)
}

/// The parts `urllib.parse.urlsplit` returns for a link, with the scheme lowercased.
#[derive(Debug)]
struct SplitUrl {
  scheme: String,
  netloc: String,
  path: String,
  query: String,
  fragment: String,
}

/// Splits a link as `urllib.parse.urlsplit` does. A bracketed network location that is malformed fails with
/// `urlsplit`'s message.
fn split_url(link: &str) -> Result<SplitUrl, String> {
  let cleaned: String =
    link.trim_start_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
  let (before_fragment, fragment) = cleaned.split_once('#').unwrap_or((cleaned.as_str(), ""));
  let (head, query) = before_fragment.split_once('?').unwrap_or((before_fragment, ""));
  let (scheme, after_scheme) = match split_scheme(head) {
    Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
    None => (String::new(), head),
  };
  let (netloc, path) = match after_scheme.strip_prefix("//") {
    Some(authority) => authority.find('/').map_or((authority, ""), |index| authority.split_at(index)),
    None => ("", after_scheme),
  };
  validate_bracketed_netloc(netloc)?;
  Ok(SplitUrl {
    scheme,
    netloc: netloc.to_string(),
    path: path.to_string(),
    query: query.to_string(),
    fragment: fragment.to_string(),
  })
}

/// The scheme and the text after its `:` when the input begins with a valid URL scheme.
fn split_scheme(link: &str) -> Option<(&str, &str)> {
  let (scheme, rest) = link.split_once(':')?;
  is_url_scheme(scheme).then_some((scheme, rest))
}

/// Whether `text` is a URL scheme: an ASCII letter, then ASCII letters, digits, `+`, `.` or `-`.
fn is_url_scheme(text: &str) -> bool {
  let mut chars = text.chars();
  chars.next().is_some_and(|first| first.is_ascii_alphabetic())
    && chars.all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '.' | '-'))
}

/// Whether `link` begins with a scheme and `://` (`_URL_SCHEME_RE`).
fn has_url_scheme(link: &str) -> bool {
  link.split_once("://").is_some_and(|(scheme, _)| is_url_scheme(scheme))
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
  percent_decode_str(value).decode_utf8_lossy().into_owned()
}

/// A Unicode decimal digit's numeric value, as Python's `int` reads it.
fn decimal_digit(character: char) -> Option<u32> {
  let codepoint = u32::from(character);
  let zero = DECIMAL_DIGIT_ZEROES.iter().rev().find(|zero| codepoint >= **zero)?;
  let digit = codepoint - *zero;
  (digit < 10).then_some(digit)
}

/// The value of a URL port written in Unicode decimal digits, as Python's `int` reads it; `None` when it exceeds `u64`.
pub fn decimal_port(text: &str) -> Option<u64> {
  text.chars().try_fold(0_u64, |value, character| {
    let digit = decimal_digit(character)?;
    value.checked_mul(10)?.checked_add(u64::from(digit))
  })
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

/// A link span of the text: the link after trailing punctuation is trimmed, and whether a Markdown target named it.
struct Candidate {
  link: String,
  markdown_target: bool,
  span: Range<usize>,
}

/// One URL or Markdown link target found in reply text, with its byte span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkTarget {
  /// The link target without Markdown delimiters or trailing URL punctuation.
  pub target: String,
  /// The target's byte span in the source text.
  pub span: Range<usize>,
}

/// Trailing punctuation that a link in prose does not keep (`LINK_TRAILING_PUNCTUATION_RE`).
fn trim_url_punctuation(link: &str) -> &str {
  link.trim_end_matches(['.', ',', ';', ':', '!', '?', '*', '。', '！', '？', '；', '，', '、', '：', '．'])
}

/// The URL and Markdown target spans of `text`, in order (`_candidates`). A span that both forms name keeps the
/// Markdown target status.
fn candidates(text: &str) -> Vec<Candidate> {
  let mut found: BTreeMap<(usize, usize), Candidate> = BTreeMap::new();
  for url in URL_RE.find_iter(text) {
    let link = trim_url_punctuation(url.as_str());
    if !link.is_empty() {
      let span = (url.start(), url.start() + link.len());
      found.insert(span, Candidate { link: link.to_string(), markdown_target: false, span: span.0..span.1 });
    }
  }
  for pattern in [&*MARKDOWN_INLINE_TARGET_RE, &*MARKDOWN_REFERENCE_TARGET_RE] {
    for captures in pattern.captures_iter(text) {
      let target = captures.name("target").expect("the pattern has a target group");
      let (raw, start) = match target.as_str().strip_prefix('<').and_then(|inner| inner.strip_suffix('>')) {
        Some(inner) => (inner, target.start() + 1),
        None => (target.as_str(), target.start()),
      };
      let link = if has_url_scheme(raw) { trim_url_punctuation(raw) } else { raw };
      if !link.is_empty() {
        let span = start..start + link.len();
        found.insert((span.start, span.end), Candidate { link: link.to_string(), markdown_target: true, span });
      }
    }
  }
  found.into_values().collect()
}

/// The retired `/files` or `/file` prefix that `path` opens, without its trailing slash (`_retired_prefix`).
fn retired_prefix(path: &str) -> Option<&'static str> {
  RETIRED_PREFIXES.into_iter().find(|prefix| path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')))
}

/// The absolute filesystem path that a file-server URL path names, percent-decoded (`_file_server_named_path`).
fn file_server_named_path(url_path: &str) -> Option<String> {
  let rest = url_path.strip_prefix(FILE_SERVER_MOUNT)?.strip_prefix('/')?;
  Some(format!("/{}", percent_decode_lossy(rest)))
}

/// The path `pathlib` makes of an absolute string: repeated and `.` components drop, and exactly two leading slashes
/// stay two (`PurePosixPath`).
fn posix_path(absolute: &str) -> PathBuf {
  let root = if absolute.starts_with("//") && !absolute.starts_with("///") { "//" } else { "/" };
  let parts: Vec<&str> = absolute.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
  PathBuf::from(format!("{root}{}", parts.join("/")))
}

/// The hostname of a network location, lowercased and without brackets, port or userinfo (`SplitResult.hostname`).
fn hostname(netloc: &str) -> String {
  let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, hostinfo)| hostinfo);
  let host = match hostinfo.strip_prefix('[') {
    Some(bracketed) => bracketed.split_once(']').map_or(bracketed, |(host, _)| host),
    None => hostinfo.split_once(':').map_or(hostinfo, |(host, _)| host),
  };
  host.to_lowercase()
}

/// The port a link names, or else its scheme's default (`_effective_port`). `None` when the explicit port exceeds
/// `u64` or the scheme has no default, and no server port equals that.
fn link_port(parts: &SplitUrl, explicit: Option<&str>) -> Option<u64> {
  match explicit {
    Some(digits) => decimal_port(digits),
    None => match parts.scheme.as_str() {
      "http" => Some(80),
      "https" => Some(443),
      _ => None,
    },
  }
}

/// The explicit port digits that `_URL_RE` reads at the start of `link` (`_candidate_port`).
fn candidate_port(link: &str) -> Option<&str> {
  let captures = URL_RE.captures(link)?;
  if captures.get(0)?.start() != 0 {
    return None;
  }
  captures.name("port").map(|port| port.as_str())
}

/// Whether a link's port is this server's port, `port == context.server_port` (false without a context).
fn names_server_port(port: Option<u64>, context: Option<&ScanContext>) -> bool {
  context.is_some_and(|context| port == Some(u64::from(context.server_port)))
}

/// Whether a canonical file-server link names this host (`_canonical_is_local`).
fn canonical_is_local(parts: &SplitUrl, port: Option<u64>, context: Option<&ScanContext>) -> bool {
  if parts.netloc.is_empty() {
    return true;
  }
  let host = hostname(&parts.netloc);
  host == "localhost" || host == "127.0.0.1" || names_server_port(port, context)
}

/// The retired link's host file and its file-server URL path, when the file exists (`_retired_path`). Without a
/// context neither is known.
fn retired_path(
  path: &str,
  prefix: &str,
  context: Option<&ScanContext>,
) -> io::Result<(Option<PathBuf>, Option<String>)> {
  let Some(context) = context else {
    return Ok((None, None));
  };
  let tail = percent_decode_lossy(&path[prefix.len() + 1..]);
  let absolute = paths::realpath(&Path::new("/").join(&tail));
  let home = paths::realpath(&context.charliebot_home);
  let home_path = paths::realpath(&home.join(&tail));
  let mut candidates = vec![absolute.clone()];
  if home_path.starts_with(&home) {
    candidates.push(home_path);
  }
  for candidate in candidates {
    if stat_path(&candidate)?.is_some() {
      let url = mount_url(&candidate);
      return Ok((Some(candidate), Some(url)));
    }
  }
  Ok((Some(absolute), None))
}

/// The file-server URL path's safe set: `urllib.parse.quote(path, safe="/:@")`.
static MOUNT_QUOTE: AsciiSet = quote_set(b"/:@");

/// The file-server URL path that names an absolute host path, quoted as `urllib.parse.quote(path, safe="/:@")` does
/// (`_encoded_file_server_url_for_parts`).
fn mount_url(path: &Path) -> String {
  let bytes = path.as_os_str().as_bytes();
  let relative = &bytes[bytes.iter().take_while(|byte| **byte == b'/').count()..];
  format!("{FILE_SERVER_MOUNT}/{}", percent_encode(relative, &MOUNT_QUOTE))
}

/// `urllib.parse.urlunsplit` of a link's scheme, network location, query and fragment around the absolute `path`.
fn unsplit(parts: &SplitUrl, path: &str) -> String {
  let mut url = path.to_string();
  if !parts.netloc.is_empty() || (USES_NETLOC.contains(&parts.scheme.as_str()) && !url.starts_with("//")) {
    url = format!("//{}{url}", parts.netloc);
  }
  if !parts.scheme.is_empty() {
    url = format!("{}:{url}", parts.scheme);
  }
  if !parts.query.is_empty() {
    url = format!("{url}?{}", parts.query);
  }
  if !parts.fragment.is_empty() {
    url = format!("{url}#{}", parts.fragment);
  }
  url
}

/// The host path a Markdown target names: a `~/` path under the user's home, a path under the session directory, or an
/// absolute path whose first segment is a directory on this host (`_local_path_target`).
fn local_path_target(parts: &SplitUrl, context: Option<&ScanContext>) -> io::Result<Option<PathBuf>> {
  let path = percent_decode_lossy(&parts.path);
  if !parts.scheme.is_empty() || !parts.netloc.is_empty() || path.is_empty() {
    return Ok(None);
  }
  let app_route = path.starts_with("/?")
    || path == "/diff"
    || path == "/api"
    || path.starts_with("/diff/")
    || path.starts_with("/api/");
  if path.starts_with(['?', '#']) || app_route {
    return Ok(None);
  }
  if path.starts_with("~/") || !path.starts_with('/') {
    let Some(context) = context else {
      return Ok(None);
    };
    let joined = match path.strip_prefix("~/") {
      Some(rest) => context.user_home.join(rest),
      None => context.session_dir.join(&path),
    };
    return Ok(Some(paths::realpath(&joined)));
  }
  let first_segment = path[1..].split_once('/').map_or(&path[1..], |(first, _)| first);
  if !first_segment.is_empty() && stat_path(&Path::new("/").join(first_segment))?.is_some_and(|m| m.is_dir()) {
    return Ok(Some(posix_path(&path)));
  }
  Ok(None)
}

/// The finding for one candidate link, or `None` when the link names no file (`_scan_candidate`).
fn scan_candidate(candidate: &Candidate, context: Option<&ScanContext>) -> io::Result<Option<FileLink>> {
  let parts = split_url(&candidate.link).map_err(io::Error::other)?;
  let port = link_port(&parts, candidate_port(&candidate.link));
  let link = candidate.link.clone();
  if let Some(named) = file_server_named_path(&parts.path) {
    let path = posix_path(&named);
    let live = if canonical_is_local(&parts, port, context) {
      Some(stat_path(&path)?.is_some_and(|metadata| metadata.is_file() || metadata.is_dir()))
    } else {
      None
    };
    return Ok(Some(FileLink { link, form: FileLinkForm::Canonical, path: Some(path), live, correct_link: None }));
  }
  if let Some(prefix) = retired_prefix(&parts.path)
    && (candidate.markdown_target || names_server_port(port, context))
  {
    let (path, correct_path) = retired_path(&parts.path, prefix, context)?;
    let correct_link = correct_path.map(|correct| unsplit(&parts, &correct));
    return Ok(Some(FileLink { link, form: FileLinkForm::RetiredPrefix, path, live: Some(false), correct_link }));
  }
  if candidate.markdown_target
    && let Some(path) = local_path_target(&parts, context)?
  {
    let correct_link = Some(mount_url(&path));
    let finding = FileLink { link, form: FileLinkForm::LocalPath, path: Some(path), live: Some(false), correct_link };
    return Ok(Some(finding));
  }
  Ok(None)
}

/// `stat` as `pathlib` uses it for `exists`, `is_file` and `is_dir`: a path with an embedded NUL byte, and the errors
/// `pathlib` ignores (ENOENT, ENOTDIR, EBADF and ELOOP), name no file.
fn stat_path(path: &Path) -> io::Result<Option<fs::Metadata>> {
  if path.as_os_str().as_bytes().contains(&0) {
    return Ok(None);
  }
  match fs::metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::EBADF | libc::ELOOP)) => {
      Ok(None)
    }
    Err(error) => Err(error),
  }
}

/// Whether `path` is a regular file, as `pathlib.Path.is_file` answers: a path that names no file, and the errors
/// `pathlib` ignores, read as no file.
pub fn is_file(path: &Path) -> io::Result<bool> {
  Ok(stat_path(path)?.is_some_and(|metadata| metadata.is_file()))
}

/// Finds the file links in `text` and classifies each one's form and liveness (`scan_file_links`). URLs with a scheme
/// are found anywhere in the text; Markdown targets can also name a local path.
pub fn scan_file_links(text: &str, context: Option<&ScanContext>) -> io::Result<Vec<FileLink>> {
  let mut findings = Vec::new();
  for candidate in candidates(text) {
    findings.extend(scan_candidate(&candidate, context)?);
  }
  Ok(findings)
}

/// Finds URL and Markdown link targets in text without classifying the targets as host files.
pub fn scan_link_targets(text: &str) -> Vec<LinkTarget> {
  let candidates = candidates(text);
  candidates
    .iter()
    .filter(|candidate| {
      candidate.markdown_target
        || !candidates.iter().any(|other| {
          other.markdown_target
            && candidate.span.start == other.span.start
            && candidate
              .link
              .strip_prefix(&other.link)
              .is_some_and(|trailing| !trailing.is_empty() && trailing.chars().all(|character| character == ')'))
        })
    })
    .map(|candidate| LinkTarget { target: candidate.link.clone(), span: candidate.span.clone() })
    .collect()
}

/// Classifies one hyperlink target, treating it as a Markdown target so retired mounts and local paths resolve.
pub fn scan_file_link_target(link: &str, context: Option<&ScanContext>) -> io::Result<Option<FileLink>> {
  let candidate = Candidate { link: link.to_string(), markdown_target: true, span: 0..link.len() };
  scan_candidate(&candidate, context)
}

/// The HTTP 422 detail for the dead findings in `findings` (`dead_links_detail`).
pub fn dead_links_detail(findings: &[FileLink]) -> String {
  let mut lines = vec!["Dead file links; nothing was recorded.".to_string()];
  for finding in findings.iter().filter(|finding| finding.dead()) {
    let reason = match finding.form {
      FileLinkForm::RetiredPrefix => "/files/ is a retired prefix.".to_string(),
      FileLinkForm::LocalPath => "a local path opens nothing on the web page.".to_string(),
      FileLinkForm::Canonical => {
        let path = finding.path.as_ref().expect("a canonical link names a path");
        format!("no file exists at {}.", path.display())
      }
    };
    let correction = finding.correct_link.as_ref().map_or(String::new(), |link| format!(" Correct link: {link}"));
    lines.push(format!("- {}: {reason}{correction}", finding.link));
  }
  lines.join("\n")
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
  fn percent_decoding_matches_urllib_unquote_with_replacement() {
    assert_eq!(percent_decode_lossy("a%20b%41%42"), "a bAB");
    assert_eq!(percent_decode_lossy("%25%2F%e9x"), "%/\u{fffd}x");
    assert_eq!(percent_decode_lossy("%C3%A9%FF"), "\u{e9}\u{fffd}");
    assert_eq!(percent_decode_lossy("100%"), "100%");
    assert_eq!(percent_decode_lossy("%zz%4"), "%zz%4");
  }

  #[test]
  fn mount_url_quotes_the_path_with_urllib_quote_and_the_mount_safe_set() {
    assert_eq!(mount_url(Path::new("/tmp/a b/\u{e9}.txt")), "/absolute_filepath/tmp/a%20b/%C3%A9.txt");
    assert_eq!(mount_url(Path::new("//x+y:z@w~_.-")), "/absolute_filepath/x%2By:z@w~_.-");
    assert_eq!(mount_url(Path::new("/a?b#c")), "/absolute_filepath/a%3Fb%23c");
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
