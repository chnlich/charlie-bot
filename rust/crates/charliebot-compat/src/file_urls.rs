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

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

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
}
