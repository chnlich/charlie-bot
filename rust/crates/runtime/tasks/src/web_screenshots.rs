//! The web UI screenshot gate of `charliebot land`: a landing whose diff changes a CharlieBot web page
//! must name a screenshot at each width in its PR body.
//!
//! A web page is any path under `web/`, or a page template under `prompts/` (`*_template.html`), which
//! renders the plan, understanding, sitrep and report pages. The PR body names each screenshot file by
//! its absolute path, and the gate reads each named PNG's pixel width from its IHDR chunk, so a file's
//! name does not decide its width. Overlap, clipping and missing controls stay with the reviewer.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, bail};
use charliebot_registry::pages::WEB_DIR_NAME;

/// The widths, in pixels, that a landing of a web page change carries a screenshot at (`SCREENSHOT_WIDTHS`).
pub const SCREENSHOT_WIDTHS: [u32; 2] = [1280, 390];

/// The first bytes of every PNG file (`PNG_SIGNATURE`).
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The bytes of a PNG file that hold the signature, the IHDR chunk header and the image width.
const PNG_HEADER_LEN: usize = 24;

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

/// Refuses a PR body that names no screenshot at a width in `SCREENSHOT_WIDTHS`. The caller decides that the
/// landing changes a web page. A named file that cannot be read or is not a PNG also refuses (`check_screenshots`).
pub fn check_screenshots(pr_body: Option<&str>) -> anyhow::Result<()> {
  let mut widths = Vec::new();
  for path in named_png_paths(pr_body.unwrap_or_default()) {
    widths.push(png_width(Path::new(path))?);
  }
  let missing: Vec<String> =
    SCREENSHOT_WIDTHS.iter().filter(|width| !widths.contains(width)).map(|width| format!("{width} px")).collect();
  if missing.is_empty() {
    return Ok(());
  }
  let required: Vec<String> = SCREENSHOT_WIDTHS.iter().map(|width| format!("{width} px")).collect();
  bail!(
    "the diff changes a web page, so the PR body must name a screenshot of the changed state at each of {} by \
     absolute path; the body names none at {}",
    required.join(" and "),
    missing.join(" and ")
  )
}

/// The absolute `.png` paths a PR body names, in the order the body names them (`named_png_paths`).
fn named_png_paths(body: &str) -> Vec<&str> {
  body
    .split_whitespace()
    .map(|token| token.trim_matches(PATH_WRAPPERS))
    .filter(|token| token.starts_with('/') && token.ends_with(".png"))
    .collect()
}

/// The pixel width of a PNG file, read from its IHDR chunk (`png_width`).
fn png_width(path: &Path) -> anyhow::Result<u32> {
  let mut header = [0u8; PNG_HEADER_LEN];
  std::fs::File::open(path)
    .and_then(|mut file| file.read_exact(&mut header))
    .with_context(|| format!("cannot read screenshot {}", path.display()))?;
  if header[..8] != PNG_SIGNATURE || header[12..16] != *b"IHDR" {
    bail!("screenshot {} is not a PNG file", path.display());
  }
  Ok(u32::from_be_bytes([header[16], header[17], header[18], header[19]]))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Writes a PNG file whose IHDR chunk carries `width` and a one-pixel height; the rest of the file is omitted.
  fn write_png(dir: &Path, name: &str, width: u32) -> String {
    let mut bytes = PNG_SIGNATURE.to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("screenshot is written");
    path.display().to_string()
  }

  #[test]
  fn a_web_page_change_without_named_screenshots_is_refused() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let desktop = write_png(dir.path(), "desktop.png", 1280);
    let refusal = check_screenshots(Some(&format!("Only the desktop shot: `{desktop}`."))).unwrap_err().to_string();
    assert!(refusal.contains("390 px"), "{refusal}");
    assert!(check_screenshots(None).is_err());
  }

  #[test]
  fn a_web_page_change_naming_both_widths_passes_and_wrapped_paths_count() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let desktop = write_png(dir.path(), "shot-a.png", 1280);
    let phone = write_png(dir.path(), "shot-b.png", 390);
    let body = format!("Tray open at 1280 px: `{desktop}`.\n- Tray open at 390 px: ({phone})");
    check_screenshots(Some(&body)).expect("both widths are named");
  }

  #[test]
  fn a_named_file_that_is_not_a_png_refuses_the_landing() {
    let dir = tempfile::tempdir().expect("temporary directory is available");
    let text = dir.path().join("not-an-image.png");
    std::fs::write(&text, b"this text file is longer than the PNG header").expect("file is written");
    let refusal = check_screenshots(Some(&text.display().to_string())).unwrap_err().to_string();
    assert!(refusal.contains("not a PNG"), "{refusal}");
  }

  #[test]
  fn only_web_pages_need_screenshots() {
    assert!(changes_web_page(&["web/app.js".to_string()]));
    assert!(changes_web_page(&["prompts/plan_template.html".to_string()]));
    assert!(!changes_web_page(&["rust/crates/x/src/lib.rs".to_string(), "prompts/master.md".to_string()]));
    assert!(!changes_web_page(&["prompts/sub/plan_template.html".to_string()]));
  }
}
