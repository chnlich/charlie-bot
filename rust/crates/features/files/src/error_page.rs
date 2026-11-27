//! The readable page that answers a path the file server cannot show: a directory or an entry that the server process
//! has no permission to read, and an entry that is not a regular file. The page names the path and the reason, and it
//! links to the parent directory, so the reader can go on from the refusal, except when the request is at the mount
//! root, where the page has no parent link.

use axum::response::Response;
use charliebot_compat::responses;
use charliebot_html::{EscapeContext, escape};
use http::StatusCode;

use crate::document::document;
use crate::listing::quote_path;
use crate::mount::MOUNT;

/// Why the file server refuses a path. Each reason has its own sentence on the page.
#[derive(Clone, Copy)]
pub(crate) enum Refusal {
  /// A directory whose entries the server process cannot list.
  DirectoryDenied,
  /// An entry whose metadata or contents the server process cannot read.
  EntryDenied,
  /// An entry that is neither a directory nor a regular file, such as a device or a pipe.
  NotRegularFile,
}

impl Refusal {
  /// The sentence that the page gives as the reason.
  pub(crate) fn reason(self) -> &'static str {
    match self {
      Self::DirectoryDenied => "Permission denied: the file server cannot list this directory.",
      Self::EntryDenied => "Permission denied: the file server cannot read this entry.",
      Self::NotRegularFile => "Not a regular file: the file server shows only files and directories.",
    }
  }
}

/// The answer that refuses `display_path`, which the request reached under `url_prefix`: a 403 whose page names the
/// path and the reason, and links to the parent directory, except when the request is at the mount root, where the page
/// has no parent link.
pub(crate) fn refusal(refusal: Refusal, display_path: &str, url_prefix: &str) -> Response {
  let path = escape(display_path, EscapeContext::Text);
  let base = url_prefix.trim_end_matches('/');
  let parent = if base == MOUNT {
    String::new()
  } else {
    let (parent, _) = base.rsplit_once('/').expect("a listed URL runs under the mount, so it holds a slash");
    format!(
      r#"<p><a class="parent" href="{}">Parent directory</a></p>"#,
      escape(&quote_path(parent), EscapeContext::QuotedAttribute)
    )
  };
  let body = format!("<h2>{path}</h2>\n<p>{}</p>\n{parent}", refusal.reason());
  let mut response = responses::html(document(&format!("Cannot open {path}"), &body));
  *response.status_mut() = StatusCode::FORBIDDEN;
  response
}
