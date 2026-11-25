//! The route check: whether a page link names an application route, a host file or an external page.

use std::fs;
use std::path::PathBuf;

use charliebot_compat::file_urls::{self, FileLinkForm};
use url::Url;

/// A page link's route after it passes `check_page_link`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageLinkTarget {
  NoLink,
  ApplicationRoute,
  External,
  HostFile(PathBuf),
}

/// The route a status, reply or rendered page can open as a browser page. An error carries the refusal reason.
pub fn check_page_link(link: Option<&str>) -> Result<PageLinkTarget, String> {
  let Some(link) = link else {
    return Ok(PageLinkTarget::NoLink);
  };
  if is_application_route(link) {
    return Ok(PageLinkTarget::ApplicationRoute);
  }

  let file_link =
    file_urls::scan_file_link_target(link, None).map_err(|error| format!("link {link:?} cannot be parsed: {error}"))?;
  if file_link.as_ref().is_some_and(|file| file.form == FileLinkForm::RetiredPrefix) {
    return Err(format!("link {link:?} uses a retired file-server prefix"));
  }

  let url = Url::parse(link).map_err(|error| format!("link {link:?} is not a valid absolute URL: {error}"))?;
  if !matches!(url.scheme(), "http" | "https") {
    return Err(format!("link {link:?} uses unsupported scheme {:?}; only http and https URLs render", url.scheme()));
  }

  let Some(path) =
    file_urls::host_file(Some(link)).map_err(|error| format!("link {link:?} cannot be parsed: {error}"))?
  else {
    return Ok(PageLinkTarget::External);
  };
  let metadata =
    fs::metadata(&path).map_err(|error| format!("mapped link {link:?} cannot read {}: {error}", path.display()))?;
  if ["md", "diff", "patch"]
    .iter()
    .any(|suffix| path.extension().is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case(suffix)))
  {
    return Err(format!("mapped link {link:?} targets a non-page file at {}", path.display()));
  }
  if metadata.is_file() {
    fs::File::open(&path).map_err(|error| format!("mapped link {link:?} cannot read {}: {error}", path.display()))?;
  } else if metadata.is_dir() {
    for entry in
      fs::read_dir(&path).map_err(|error| format!("mapped link {link:?} cannot read {}: {error}", path.display()))?
    {
      entry.map_err(|error| format!("mapped link {link:?} cannot read {}: {error}", path.display()))?;
    }
  } else {
    return Err(format!("mapped link {link:?} does not name a rendered file or directory at {}", path.display()));
  }
  Ok(PageLinkTarget::HostFile(path))
}

/// Whether a local link names one of the application routes that does not map to a host file.
fn is_application_route(link: &str) -> bool {
  if link.starts_with("/?") {
    return true;
  }
  let path = link.split(['?', '#']).next().expect("split always yields the full string");
  file_urls::is_application_route(path)
}
