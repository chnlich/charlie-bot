//! The web UI screenshot gate of `charliebot land`, `charliebot screenshots check` and an improve round's
//! merge: a change that alters a CharlieBot web page must carry a screenshot of the changed state at each
//! window size in [`WINDOW_SIZES`].
//!
//! A web page change is a changed path that the server serves or renders, or a page template directly under
//! `prompts/` (`*_template.html`), which the plan, understanding, sitrep and report pages follow. The served set
//! is the registry's (`pages::is_served_path`), so a new page package under `web/features/` or `web/backends/` is
//! counted without an edit here. The counted set is:
//!
//! - `web/templates/**` and `web/static/**`, such as `web/static/js/websocket.js` and `web/templates/index.html`;
//! - each page package's `templates/**` and `static/**` under `web/features/<name>` or `web/backends/<name>`, such
//!   as `web/features/cron/templates/cron/page_modals.html`, `web/features/cron/static/cron.js` and
//!   `web/backends/codex/templates/codex/codex.css`;
//! - `prompts/*_template.html`, such as `prompts/plan_template.html`.
//!
//! The exempt set passes the gate: every other path under `web/`, such as `web/tests/**`, `web/package.json`,
//! `web/package-lock.json`, `web/tailwind*.config.js` and a package's own file outside its `templates/` and
//! `static/`, such as `web/backends/opencode/cgroup_join.sh`. The Tailwind configs are exempt because
//! `dev/build-css.sh` writes the served stylesheets under `web/static/css/` by hand, and those committed files are
//! counted: a config change serves no byte until its regenerated CSS lands.
//!
//! Screenshots are captured at device pixel ratio 1, so a PNG's pixel size equals its CSS window size; the gate
//! reads the width and height from the PNG's IHDR chunk, so a file's name does not decide its size. Overlap,
//! clipping and missing controls stay with the reviewer.
//!
//! An improve round's merge also needs a review record that shows the review read each screenshot the PR body names
//! by its file name (`check_review_reads`). The record is the `events.ndjson` that `charliebot backend run` writes in
//! the review's working directory; a read counts when its `Read` call's result was not an error.
//!
//! A landing's PR body also names the approved demo (`Demo: <absolute path>`, whose file must exist) and a
//! `View: <URL>` line naming the full URL, path and query, that opens the changed state. A landing carries each View
//! line into the landed commit as a trailer, which `scripts/switch-server.sh` reads back to capture the live page
//! after the switch.
//!
//! A view resolves on the live server when `check_view_resolves` passes it: a `/?session=<id>` view when the server
//! knows the session, any other view on a 2xx answer. The landing refuses a session view the server does not know.
//! The switch refuses each view that does not resolve, before it captures that view.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_cli_common::{client, route};
use charliebot_core::event_types::{
  IS_ERROR, MESSAGE_CONTENT_POINTER, READ_TOOL, TOOL_INPUT_FILE_PATH, TOOL_RESULT, TOOL_USE, TOOL_USE_ID,
};
use charliebot_registry::pages::is_served_path;
use serde_json::Value;

/// The CSS window sizes, at device pixel ratio 1, that a screenshot of a changed page shows: a 1920 wide
/// screen at 125% zoom (`1536`), a narrower desktop window, and a phone. A capture at another scale, such as
/// a 125% zoom's 1920 px width, matches no size.
pub const WINDOW_SIZES: [(u32, u32); 3] = [(1536, 900), (1400, 1000), (390, 844)];

/// The screenshots each window size needs from a landing: one, the changed state.
pub const LANDING_SHOTS_PER_SIZE: usize = 1;

/// The screenshots each window size needs from an improve round: two, the state before and after its change.
pub const ROUND_SHOTS_PER_SIZE: usize = 2;

/// The first bytes of every PNG file (`PNG_SIGNATURE`).
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The bytes of a PNG file that hold the signature, the IHDR chunk header and the image's width and height.
const PNG_HEADER_LEN: usize = 24;

/// The line prefix that names the approved demo in a PR body.
const DEMO_PREFIX: &str = "Demo:";

/// The line prefix for a `View: <URL>` line naming the full URL, path and query, that opens the changed state.
const VIEW_PREFIX: &str = "View:";

/// The punctuation that wraps a path in prose or a Markdown list, which a named path sheds (`PATH_WRAPPERS`).
const PATH_WRAPPERS: &[char] = &['`', '\'', '"', '(', ')', '[', ']', '<', '>', ',', ';', ':', '.'];

/// Whether a changed path alters a CharlieBot web page: a file the server serves or renders (`is_served_path`), or
/// a page template directly under `prompts/` (`changes_web_page`).
pub fn is_web_page_path(path: &str) -> bool {
  is_served_path(path)
    || path.strip_prefix("prompts/").is_some_and(|name| !name.contains('/') && name.ends_with("_template.html"))
}

/// Whether a landing's changed paths alter a CharlieBot web page, so the landing needs screenshots.
pub fn changes_web_page(changed_paths: &[String]) -> bool {
  changed_paths.iter().any(|path| is_web_page_path(path))
}

/// The window sizes as help and refusal text writes them, e.g. `1536x900, 1400x1000, 390x844`.
pub fn window_sizes_text() -> String {
  WINDOW_SIZES.iter().map(|size| size_label(*size)).collect::<Vec<_>>().join(", ")
}

/// What a web page change's PR body promises: the approved demo's file, and the view paths to capture after
/// the release switch (`check_pr_body`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyContract {
  pub demo: PathBuf,
  pub views: Vec<String>,
}

/// Refuses a web page change's PR body unless it names `Demo: <absolute path>` whose file exists,
/// a `View: <URL>` line naming the full URL, path and query, that opens the changed state, and a PNG at each window
/// size by absolute path, and unless each `/?session=<id>` view it
/// names is a session the live server knows. The caller decides that the change alters a web page. A named file
/// that cannot be read or is not a PNG also refuses (`check_pr_body`).
pub fn check_pr_body(pr_body: Option<&str>) -> anyhow::Result<BodyContract> {
  // The live lookups block on a runtime of their own, which cannot start on the thread that drives `land`, so they
  // run on a thread of their own.
  std::thread::scope(|scope| scope.spawn(|| check_pr_body_with(pr_body, &live_status)).join())
    .map_err(|_| anyhow::anyhow!("the live server check of the PR body panicked"))?
}

/// `check_pr_body` with the server's answers read from `status_of`.
fn check_pr_body_with(pr_body: Option<&str>, status_of: &StatusOf) -> anyhow::Result<BodyContract> {
  let body = pr_body.unwrap_or_default();
  let required = format!(
    "the diff changes a web page, so the PR body must name the approved demo on a `Demo: <absolute path>` line, a \
     `View: <URL>` line naming the full URL, path and query, that opens the changed state, and a screenshot at each \
     CSS window size ({}) by absolute \
     path at device pixel ratio 1",
    window_sizes_text()
  );
  let demo = line_value(body, DEMO_PREFIX)
    .filter(|value| !value.is_empty())
    .ok_or_else(|| anyhow::anyhow!("{required}; the body has no Demo line"))?;
  if !demo.starts_with('/') {
    bail!("{required}; the body's Demo line names {demo}, which is not an absolute path");
  }
  let demo = PathBuf::from(demo);
  if !demo.is_file() {
    bail!("{required}; the body's Demo line names {}, which is not a file", demo.display());
  }
  let views = line_values(body, VIEW_PREFIX);
  if views.is_empty() {
    bail!("{required}; the body has no View line");
  }
  for view in &views {
    if !view.starts_with('/') {
      bail!("{required}; the body's View line names {view}, which is not a URL path starting with `/`");
    }
  }
  let named: Vec<PathBuf> = named_png_paths(body).into_iter().map(PathBuf::from).collect();
  check_screenshot_files(&named, LANDING_SHOTS_PER_SIZE)?;
  check_session_views(&views, status_of)?;
  Ok(BodyContract { demo, views })
}

/// The HTTP status the server answers for one GET of a path.
pub type StatusOf = dyn Fn(&str) -> anyhow::Result<u16> + Sync;

/// The live server's status for one GET of `path`, through the CLI's server client and its credential.
pub fn live_status(path: &str) -> anyhow::Result<u16> {
  client::get_status(path).map_err(|error| anyhow::anyhow!("cannot reach the live server: {}", error.body))
}

/// The session id a `/?session=<id>` view names: the `session` query of the index page, which opens that session.
pub fn session_of_view(view: &str) -> Option<&str> {
  view.strip_prefix("/?")?.split('&').find_map(|pair| pair.strip_prefix("session="))
}

/// Whether `session` has the shape of a session id: letters, digits, `-` and `_`. The id becomes a path segment.
fn is_session_id(session: &str) -> bool {
  !session.is_empty() && session.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Refuses a view that the live server does not resolve, with the server's answers read from `status_of`.
///
/// A `/?session=<id>` view resolves when the server knows the session, which its session route answers with a 2xx.
/// The index page answers 200 for any id, so the page's own status proves nothing. Any other view resolves on a 2xx
/// answer to its path.
pub fn check_view_resolves(view: &str, status_of: &StatusOf) -> anyhow::Result<()> {
  let (path, session) = match session_of_view(view) {
    Some(session) if is_session_id(session) => (route::fill(route::SESSION_PATH, &[session]), Some(session)),
    Some(session) => {
      bail!("the View line `{view}` must name a view on the live server, but `{session}` is not a session id")
    }
    None => (view.to_string(), None),
  };
  let status = status_of(&path)?;
  if (200..300).contains(&status) {
    return Ok(());
  }
  let reason = match session {
    Some(session) if status == 404 => format!("the live server does not know session {session}"),
    _ => format!("GET {path} answered {status}, not 2xx"),
  };
  bail!("the View line `{view}` must name a view on the live server, but {reason}")
}

/// Refuses each `/?session=<id>` view the live server does not resolve. A path the PR adds cannot resolve before the
/// switch, so the landing checks session ids only (`check_pr_body`).
fn check_session_views(views: &[String], status_of: &StatusOf) -> anyhow::Result<()> {
  for view in views.iter().filter(|view| session_of_view(view).is_some()) {
    check_view_resolves(view, status_of)?;
  }
  Ok(())
}

/// Refuses PNG paths that do not cover each window size `count_per_size` times. A path that cannot be read
/// or is not a PNG refuses by name (`check_screenshot_files`).
pub fn check_screenshot_files(paths: &[PathBuf], count_per_size: usize) -> anyhow::Result<()> {
  let mut sizes = Vec::with_capacity(paths.len());
  for path in paths {
    sizes.push((path.clone(), png_size(path)?));
  }
  check_size_coverage(&sizes, count_per_size, "the PR body's screenshot paths")
}

/// Refuses a directory that does not hold `count_per_size` PNG files of each window size
/// (`check_screenshot_dir`).
pub fn check_screenshot_dir(dir: &Path, count_per_size: usize) -> anyhow::Result<()> {
  let mut sizes = Vec::new();
  let entries =
    std::fs::read_dir(dir).with_context(|| format!("cannot read screenshot directory {}", dir.display()))?;
  for entry in entries {
    let path = entry.with_context(|| format!("cannot read screenshot directory {}", dir.display()))?.path();
    if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("png")) {
      sizes.push((path.clone(), png_size(&path)?));
    }
  }
  check_size_coverage(&sizes, count_per_size, &format!("screenshot directory {}", dir.display()))
}

/// Refuses a review record that shows no successful `Read` of a screenshot that the PR body names, and a PR body that
/// names no screenshot, because the review then has no screenshot it must read (`check_review_reads`).
pub fn check_review_reads(record: &Path, named: &[String]) -> anyhow::Result<()> {
  if named.is_empty() {
    bail!(
      "the PR body names no screenshot file, so the review has no screenshot to read; name each screenshot by its file \
       name"
    );
  }
  let reads = review_read_names(record)?;
  let unread: Vec<&str> = named.iter().map(String::as_str).filter(|name| !reads.contains(*name)).collect();
  if unread.is_empty() {
    return Ok(());
  }
  bail!(
    "the review record {} shows no read of {}; the review must read each screenshot the PR body names",
    record.display(),
    unread.join(", ")
  )
}

/// The file names that a review record shows the review read: each `Read` call whose result was not an error. The
/// record is the raw event stream of a Claude run. Its assistant events carry the `tool_use` blocks, and its user
/// events carry the `tool_result` block that answers each one by its `tool_use_id` (`review_read_names`).
fn review_read_names(record: &Path) -> anyhow::Result<BTreeSet<String>> {
  let text =
    std::fs::read_to_string(record).with_context(|| format!("cannot read the review record {}", record.display()))?;
  let mut reads: Vec<(String, String)> = Vec::new();
  let mut succeeded: BTreeSet<String> = BTreeSet::new();
  for (index, line) in text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty()) {
    let event: Value = serde_json::from_str(line)
      .with_context(|| format!("line {} of the review record {} is not JSON", index + 1, record.display()))?;
    let Some(blocks) = event.pointer(MESSAGE_CONTENT_POINTER).and_then(Value::as_array) else {
      continue;
    };
    for block in blocks {
      match block.get("type").and_then(Value::as_str) {
        Some(TOOL_USE) if block.get("name").and_then(Value::as_str) == Some(READ_TOOL) => {
          let id = block.get("id").and_then(Value::as_str);
          let file = block.get("input").and_then(|input| input.get(TOOL_INPUT_FILE_PATH)).and_then(Value::as_str);
          if let (Some(id), Some(file)) = (id, file) {
            reads.push((id.to_string(), file_name(file).to_string()));
          }
        }
        Some(TOOL_RESULT) if block.get(IS_ERROR).and_then(Value::as_bool) != Some(true) => {
          if let Some(id) = block.get(TOOL_USE_ID).and_then(Value::as_str) {
            succeeded.insert(id.to_string());
          }
        }
        _ => {}
      }
    }
  }
  Ok(reads.into_iter().filter(|(id, _)| succeeded.contains(id)).map(|(_, name)| name).collect())
}

/// Refuses sizes that do not cover each window size `count_per_size` times, and names each size the files
/// hold outside the window sizes and each window size that falls short (`check_size_coverage`).
fn check_size_coverage(sizes: &[(PathBuf, (u32, u32))], count_per_size: usize, what: &str) -> anyhow::Result<()> {
  let held = |window: (u32, u32)| sizes.iter().filter(|(_, size)| *size == window).count();
  let short: Vec<(u32, u32)> = WINDOW_SIZES.iter().copied().filter(|window| held(*window) < count_per_size).collect();
  let outside: Vec<String> = sizes
    .iter()
    .filter(|(_, size)| !WINDOW_SIZES.contains(size))
    .map(|(path, size)| format!("{} ({})", path.display(), size_label(*size)))
    .collect();
  if short.is_empty() && outside.is_empty() {
    return Ok(());
  }
  let noun = if count_per_size == 1 { "screenshot" } else { "screenshots" };
  let mut refusal = format!(
    "{what} must carry {count_per_size} {noun} at each CSS window size ({}) captured at device pixel ratio 1; it \
     carries {}",
    window_sizes_text(),
    WINDOW_SIZES
      .iter()
      .map(|window| format!("{} at {}", held(*window), size_label(*window)))
      .collect::<Vec<_>>()
      .join(", ")
  );
  if !short.is_empty() {
    refusal.push_str(&format!(
      "; it needs {count_per_size} at {}",
      short.iter().map(|window| size_label(*window)).collect::<Vec<_>>().join(", ")
    ));
  }
  if !outside.is_empty() {
    refusal.push_str(&format!("; sizes outside the set: {}", outside.join(", ")));
  }
  bail!("{refusal}")
}

/// The value of the first `Demo:`-style line in a PR body, with the punctuation that wraps it shed
/// (`line_value`).
fn line_value<'a>(body: &'a str, prefix: &str) -> Option<&'a str> {
  body.lines().find_map(|line| line.trim().strip_prefix(prefix).map(|rest| rest.trim().trim_matches(PATH_WRAPPERS)))
}

/// Every `View:`-style line's value in a PR body, in the order the body names them (`line_values`).
fn line_values(body: &str, prefix: &str) -> Vec<String> {
  body
    .lines()
    .filter_map(|line| line.trim().strip_prefix(prefix))
    .map(|rest| rest.trim().trim_matches(PATH_WRAPPERS).to_string())
    .filter(|value| !value.is_empty())
    .collect()
}

/// The absolute `.png` paths a PR body names, in the order the body names them. The `Demo:` line is left
/// out: the demo shows the page before the change, so it is not a screenshot of the changed state
/// (`named_png_paths`).
fn named_png_paths(body: &str) -> Vec<&str> {
  body
    .lines()
    .filter(|line| !line.trim_start().starts_with(DEMO_PREFIX))
    .flat_map(str::split_whitespace)
    .map(|token| token.trim_matches(PATH_WRAPPERS))
    .filter(|token| token.starts_with('/') && token.ends_with(".png"))
    .collect()
}

/// The screenshot file names that a PR body names: each token that ends in `.png`, by its file name, sorted and without
/// repeats. A round's body names each screenshot by its file name, where a landing names the absolute path
/// (`named_png_paths`).
pub fn named_screenshots(pr_body: &str) -> Vec<String> {
  pr_body
    .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
    .map(|token| token.trim_end_matches('.'))
    .filter(|token| token.ends_with(".png"))
    .map(|token| file_name(token).to_string())
    .collect::<BTreeSet<String>>()
    .into_iter()
    .collect()
}

/// The part of a path after its last `/`, or the whole text when it holds no `/` (`file_name`).
fn file_name(path: &str) -> &str {
  path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// One window size's label, e.g. `1536x900` (`size_label`).
fn size_label(size: (u32, u32)) -> String {
  format!("{}x{}", size.0, size.1)
}

/// The pixel width and height of a PNG file, read from its IHDR chunk (`png_size`).
fn png_size(path: &Path) -> anyhow::Result<(u32, u32)> {
  let mut header = [0u8; PNG_HEADER_LEN];
  std::fs::File::open(path)
    .and_then(|mut file| file.read_exact(&mut header))
    .with_context(|| format!("cannot read screenshot {}", path.display()))?;
  if header[..8] != PNG_SIGNATURE || header[12..16] != *b"IHDR" {
    bail!("screenshot {} is not a PNG file", path.display());
  }
  let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
  let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
  Ok((width, height))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Writes a PNG file whose IHDR chunk carries `width` and `height`; the rest of the file is omitted.
  fn write_png(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    let mut bytes = PNG_SIGNATURE.to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("screenshot is written");
    path
  }

  /// One PNG at each window size, named `shot-<index>`.
  fn write_window_shots(dir: &Path, first: usize, count: usize) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for (offset, (width, height)) in WINDOW_SIZES.iter().enumerate() {
      for copy in 0..count {
        paths.push(write_png(dir, &format!("shot-{}.png", first + offset * count + copy), *width, *height));
      }
    }
    paths
  }

  #[test]
  fn a_body_without_a_demo_line_is_refused() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let shots = write_window_shots(dir.path(), 0, 1);
    let named: Vec<String> = shots.iter().map(|path| path.display().to_string()).collect();
    let body = format!("View: /?session=abc\n{}", named.join(" "));
    let refusal = check_pr_body(Some(&body)).unwrap_err().to_string();
    assert!(refusal.contains("Demo"), "{refusal}");
    assert!(refusal.contains("1536x900"), "{refusal}");
  }

  #[test]
  fn a_body_whose_demo_is_missing_is_refused() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let shots = write_window_shots(dir.path(), 0, 1);
    let named: Vec<String> = shots.iter().map(|path| path.display().to_string()).collect();
    let body = format!("Demo: /absent/demo.html\nView: /status\n{}", named.join(" "));
    let refusal = check_pr_body(Some(&body)).unwrap_err().to_string();
    assert!(refusal.contains("/absent/demo.html"), "{refusal}");
  }

  #[test]
  fn a_body_without_a_view_line_is_refused() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let demo = write_png(dir.path(), "demo.png", 10, 10);
    let shots = write_window_shots(dir.path(), 0, 1);
    let named: Vec<String> = shots.iter().map(|path| path.display().to_string()).collect();
    let body = format!("Demo: {}\n{}", demo.display(), named.join(" "));
    let refusal = check_pr_body(Some(&body)).unwrap_err().to_string();
    assert!(
      refusal.contains("a `View: <URL>` line naming the full URL, path and query, that opens the changed state"),
      "{refusal}"
    );
  }

  /// A server that answers every GET with `status`.
  fn answers(status: u16) -> impl Fn(&str) -> anyhow::Result<u16> + Sync {
    move |_| Ok(status)
  }

  /// The live server's answers for a session it does not know: the index page answers 200 for every id, and the
  /// session route answers 404.
  fn unknown_session_server(path: &str) -> anyhow::Result<u16> {
    Ok(if path.starts_with("/api/sessions/") { 404 } else { 200 })
  }

  /// A PR body with a demo, one PNG at each window size, and `view_lines` as its View lines.
  fn body_with_views(dir: &Path, view_lines: &str) -> String {
    let demo = write_png(dir, "demo.png", 10, 10);
    let shots = write_window_shots(dir, 0, 1);
    let named: Vec<String> = shots.iter().map(|path| path.display().to_string()).collect();
    format!("Demo: {}\n{view_lines}\nShots: `{}`", demo.display(), named.join("` `"))
  }

  #[test]
  fn a_body_naming_one_screenshot_at_each_window_size_passes() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let body = body_with_views(dir.path(), "View: /?session=abc");
    let contract = check_pr_body_with(Some(&body), &answers(200)).expect("each window size is named");
    assert_eq!(contract.demo, dir.path().join("demo.png"));
    assert_eq!(contract.views, vec!["/?session=abc".to_string()]);
    assert!(check_pr_body(None).is_err());
  }

  #[test]
  fn a_body_naming_an_unknown_session_is_refused_with_the_reason() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let body = body_with_views(dir.path(), "View: /?session=5f1c0d2e-7a3b");
    let refusal = check_pr_body_with(Some(&body), &unknown_session_server).unwrap_err().to_string();
    assert!(refusal.contains("must name a view on the live server"), "{refusal}");
    assert!(refusal.contains("does not know session 5f1c0d2e-7a3b"), "{refusal}");
  }

  #[test]
  fn a_body_whose_path_view_is_not_probed_before_the_switch() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let body = body_with_views(dir.path(), "View: /plans");
    let probe_refused = |path: &str| -> anyhow::Result<u16> { bail!("the path view was probed at {path}") };
    check_pr_body_with(Some(&body), &probe_refused).expect("only session ids are checked at landing");
  }

  #[test]
  fn a_session_view_the_server_does_not_know_is_refused_whatever_the_index_page_answers() {
    let refusal = check_view_resolves("/?session=5f1c0d2e", &unknown_session_server).unwrap_err().to_string();
    assert!(refusal.contains("does not know session 5f1c0d2e"), "{refusal}");
  }

  #[test]
  fn a_session_view_the_server_knows_resolves_through_its_session_route() {
    let known_route = |path: &str| -> anyhow::Result<u16> {
      assert_eq!(path, "/api/sessions/abc");
      Ok(200)
    };
    check_view_resolves("/?session=abc", &known_route).expect("a known session resolves");
  }

  #[test]
  fn a_view_that_names_no_session_is_refused_without_a_probe() {
    let probe_refused = |path: &str| -> anyhow::Result<u16> { bail!("the view was probed at {path}") };
    for view in ["/?session=", "/?session=a/../b"] {
      let refusal = check_view_resolves(view, &probe_refused).unwrap_err().to_string();
      assert!(refusal.contains("is not a session id"), "{refusal}");
    }
  }

  #[test]
  fn a_path_view_resolves_on_a_2xx_answer_only() {
    check_view_resolves("/plans", &answers(200)).expect("a served page resolves");
    let refusal = check_view_resolves("/plans", &answers(404)).unwrap_err().to_string();
    assert!(refusal.contains("GET /plans answered 404"), "{refusal}");
  }

  #[test]
  fn a_body_naming_no_screenshot_is_refused_by_size() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let demo = write_png(dir.path(), "demo.png", 10, 10);
    let body = format!("Demo: {}\nView: /", demo.display());
    let refusal = check_pr_body(Some(&body)).unwrap_err().to_string();
    assert!(refusal.contains("1536x900"), "{refusal}");
    assert!(refusal.contains("390x844"), "{refusal}");
  }

  #[test]
  fn a_png_of_the_right_width_and_wrong_height_is_refused_and_named() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let mut shots = write_window_shots(dir.path(), 0, 1);
    shots[0] = write_png(dir.path(), "wrong.png", 1440, 1600);
    let refusal = check_screenshot_files(&shots, 1).unwrap_err().to_string();
    assert!(refusal.contains("wrong.png (1440x1600)"), "{refusal}");
    assert!(refusal.contains("1536x900"), "{refusal}");
  }

  #[test]
  fn a_directory_needs_two_screenshots_of_each_size_for_a_round() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    write_window_shots(dir.path(), 0, 1);
    let refusal = check_screenshot_dir(dir.path(), ROUND_SHOTS_PER_SIZE).unwrap_err().to_string();
    assert!(refusal.contains("needs 2 at 1536x900"), "{refusal}");
    write_window_shots(dir.path(), 10, 1);
    check_screenshot_dir(dir.path(), ROUND_SHOTS_PER_SIZE).expect("two of each size pass");
  }

  #[test]
  fn a_named_file_that_is_not_a_png_refuses_the_landing() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let text = dir.path().join("not-an-image.png");
    std::fs::write(&text, b"this text file is longer than the PNG header").expect("file is written");
    let refusal = check_screenshot_files(&[text], 1).unwrap_err().to_string();
    assert!(refusal.contains("not a PNG"), "{refusal}");
  }

  #[test]
  fn only_served_pages_need_screenshots() {
    for served in [
      "web/static/js/websocket.js",
      "web/templates/index.html",
      "web/features/cron/templates/cron/page_modals.html",
      "web/features/cron/static/cron.js",
      "web/backends/codex/templates/codex/codex.css",
      "prompts/plan_template.html",
    ] {
      assert!(changes_web_page(&[served.to_string()]), "{served} is served");
    }
    for exempt in [
      "web/tests/websocket_reconnect_poke.test.js",
      "web/package.json",
      "web/backends/opencode/cgroup_join.sh",
      "web/tailwind.config.js",
      "rust/crates/x/src/lib.rs",
      "prompts/master.md",
      "prompts/sub/plan_template.html",
    ] {
      assert!(!changes_web_page(&[exempt.to_string()]), "{exempt} is not served");
    }
  }

  #[test]
  fn the_window_sizes_text_names_every_size() {
    assert_eq!(window_sizes_text(), "1536x900, 1400x1000, 390x844");
  }

  /// A review record that reads each file in `reads`: a `Read` call on an assistant event, answered on the next user
  /// event by a `tool_result` that is an error when the file is in `errors`.
  fn review_record(dir: &Path, reads: &[&str], errors: &[&str]) -> PathBuf {
    let mut lines = Vec::new();
    for (index, name) in reads.iter().enumerate() {
      let id = format!("toolu_{index}");
      lines.push(
        serde_json::json!({"type": "assistant", "message": {"content": [
          {"type": "tool_use", "id": id.clone(), "name": "Read", "input": {"file_path": format!("/evidence/{name}")}}
        ]}})
        .to_string(),
      );
      lines.push(
        serde_json::json!({"type": "user", "message": {"content": [
          {"type": "tool_result", "tool_use_id": id, "is_error": errors.contains(name), "content": "ok"}
        ]}})
        .to_string(),
      );
    }
    let path = dir.join("events.ndjson");
    std::fs::write(&path, lines.join("\n") + "\n").expect("the record is written");
    path
  }

  #[test]
  fn the_body_names_each_screenshot_by_its_file_name_once() {
    let body = "- Before, 1536x900: `voice-before-idle-1536x900.png`\n\
                - After, 1536x900: voice-after-idle-1536x900.png.\n\
                - Again: evidence/voice-before-idle-1536x900.png\n\
                Demo: /tmp/demo.html\n";

    assert_eq!(named_screenshots(body), ["voice-after-idle-1536x900.png", "voice-before-idle-1536x900.png"]);
  }

  #[test]
  fn a_review_that_read_every_named_screenshot_passes() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let record = review_record(dir.path(), &["a.png", "b.png"], &[]);

    check_review_reads(&record, &["a.png".to_string(), "b.png".to_string()]).expect("both screenshots were read");
  }

  #[test]
  fn a_review_that_missed_a_named_screenshot_is_refused_by_name() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let record = review_record(dir.path(), &["a.png"], &[]);

    let refusal = check_review_reads(&record, &["a.png".to_string(), "b.png".to_string()])
      .expect_err("b.png was never read")
      .to_string();

    assert!(refusal.contains("shows no read of b.png"), "{refusal}");
  }

  #[test]
  fn a_read_whose_result_is_an_error_is_not_a_read() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let record = review_record(dir.path(), &["a.png"], &["a.png"]);

    assert!(check_review_reads(&record, &["a.png".to_string()]).is_err());
  }

  #[test]
  fn a_body_naming_no_screenshot_refuses_the_review_record() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let record = review_record(dir.path(), &["a.png"], &[]);

    let refusal = check_review_reads(&record, &[]).expect_err("no screenshot is named").to_string();

    assert!(refusal.contains("names no screenshot file"), "{refusal}");
  }
}
