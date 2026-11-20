//! The web UI screenshot gate of `charliebot land`, `charliebot screenshots check` and an improve round's
//! merge: a change that alters a CharlieBot web page must carry a screenshot of the changed state at each
//! window size in [`WINDOW_SIZES`].
//!
//! A web page is any path under `web/`, or a page template directly under `prompts/` (`*_template.html`),
//! which renders the plan, understanding, sitrep and report pages. Screenshots are captured at device pixel
//! ratio 1, so a PNG's pixel size equals its CSS window size; the gate reads the width and height from the
//! PNG's IHDR chunk, so a file's name does not decide its size. Overlap, clipping and missing controls stay
//! with the reviewer.
//!
//! A landing's PR body also names the approved demo (`Demo: <absolute path>`, whose file must exist) and the
//! changed view's URL path (`View: <URL path>`). A landing carries each View line into the landed commit as
//! a trailer, which `scripts/switch-server.sh` reads back to capture the live page after the switch.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_registry::pages::WEB_DIR_NAME;

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

/// The line prefix that names the changed view's URL path in a PR body.
const VIEW_PREFIX: &str = "View:";

/// The punctuation that wraps a path in prose or a Markdown list, which a named path sheds (`PATH_WRAPPERS`).
const PATH_WRAPPERS: &[char] = &['`', '\'', '"', '(', ')', '[', ']', '<', '>', ',', ';', ':', '.'];

/// Whether a changed path alters a CharlieBot web page: a file under `web/`, or a page template directly under
/// `prompts/` (`changes_web_page`).
pub fn is_web_page_path(path: &str) -> bool {
  path.strip_prefix(WEB_DIR_NAME).is_some_and(|rest| rest.starts_with('/'))
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
/// `View: <URL path>`, and a PNG at each window size by absolute path. The caller decides that the change
/// alters a web page. A named file that cannot be read or is not a PNG also refuses (`check_pr_body`).
pub fn check_pr_body(pr_body: Option<&str>) -> anyhow::Result<BodyContract> {
  let body = pr_body.unwrap_or_default();
  let required = format!(
    "the diff changes a web page, so the PR body must name the approved demo on a `Demo: <absolute path>` line, the \
     changed view's URL path on a `View: <URL path>` line, and a screenshot at each CSS window size ({}) by absolute \
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
  Ok(BodyContract { demo, views })
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
    assert!(refusal.contains("View"), "{refusal}");
  }

  #[test]
  fn a_body_naming_one_screenshot_at_each_window_size_passes() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let demo = write_png(dir.path(), "demo.png", 10, 10);
    let shots = write_window_shots(dir.path(), 0, 1);
    let named: Vec<String> = shots.iter().map(|path| path.display().to_string()).collect();
    let body = format!("Demo: {}\nView: /?session=abc\nShots: `{}`", demo.display(), named.join("` `"));
    let contract = check_pr_body(Some(&body)).expect("each window size is named");
    assert_eq!(contract.demo, demo);
    assert_eq!(contract.views, vec!["/?session=abc".to_string()]);
    assert!(check_pr_body(None).is_err());
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
  fn only_web_pages_need_screenshots() {
    assert!(changes_web_page(&["web/app.js".to_string()]));
    assert!(changes_web_page(&["prompts/plan_template.html".to_string()]));
    assert!(!changes_web_page(&["rust/crates/x/src/lib.rs".to_string(), "prompts/master.md".to_string()]));
    assert!(!changes_web_page(&["prompts/sub/plan_template.html".to_string()]));
  }

  #[test]
  fn the_window_sizes_text_names_every_size() {
    assert_eq!(window_sizes_text(), "1536x900, 1400x1000, 390x844");
  }
}
