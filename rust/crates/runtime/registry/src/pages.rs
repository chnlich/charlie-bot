//! Page package paths and template parts.

use std::path::{Path, PathBuf};

use charliebot_core::text_repr;

pub use crate::registry::{HomeCard, TemplateGlobalFn};

/// The places a page leaves for package parts (`page_render.PAGE_SLOTS`).
pub const PAGE_SLOTS: &[&str] = &[
  "head",
  "tab_buttons",
  "below_header",
  "tab_panels",
  "input_buttons",
  "modals",
  "scripts",
  "diff_toolbar",
  "home_notes",
];

/// `src/<kind>/<name>` for the package `src.<kind>.<name>`, under the checkout at `repo_root`.
pub fn package_directory(repo_root: &Path, package: &str) -> PathBuf {
  package.split('.').fold(repo_root.to_path_buf(), |path, part| path.join(part))
}

/// The slot's part templates that exist on disk now, as `<name>/page_<slot>.html`, in package order.
///
/// `packages` is the registry's [`Registry::page_packages`]. An unknown slot fails.
pub fn page_parts(repo_root: &Path, packages: &[&str], slot: &str) -> Result<Vec<String>, String> {
  if !PAGE_SLOTS.contains(&slot) {
    let slots = PAGE_SLOTS.iter().map(|name| text_repr::str_repr(name)).collect::<Vec<_>>().join(", ");
    return Err(format!("page slot {} is not one of ({slots})", text_repr::str_repr(slot)));
  }
  Ok(
    packages
      .iter()
      .filter_map(|package| {
        let name =
          format!("{}/page_{slot}.html", package.rsplit('.').next().expect("rsplit yields at least one piece"));
        package_directory(repo_root, package).join("templates").join(&name).is_file().then_some(name)
      })
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn page_parts_follow_the_package_registration_order() {
    let root = tempfile::tempdir().unwrap();
    let packages = ["src.features.zeta", "src.features.alpha"];
    for package in packages {
      let templates = package_directory(root.path(), package).join("templates");
      let part = package.rsplit('.').next().unwrap();
      std::fs::create_dir_all(templates.join(part)).unwrap();
      std::fs::write(templates.join(part).join("page_head.html"), "").unwrap();
    }

    let parts = page_parts(root.path(), &packages, "head").unwrap();
    assert_eq!(parts, ["zeta/page_head.html", "alpha/page_head.html"]);
  }
}
