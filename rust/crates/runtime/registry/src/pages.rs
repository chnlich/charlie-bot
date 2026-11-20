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

/// The frontend root under `repo_root`: the web app's own templates and static files sit below it.
pub const WEB_DIR_NAME: &str = "web";

/// The template directory of the web app and of each page package.
pub const TEMPLATES_DIR_NAME: &str = "templates";

/// The static file directory of the web app and of each page package.
pub const STATIC_DIR_NAME: &str = "static";

/// The first dotted part of each page package whose directory sits under the frontend root: `features.<name>` maps to
/// `web/features/<name>` and `backends.<name>` to `web/backends/<name>` (`package_directory`).
pub const WEB_PACKAGE_KINDS: &[&str] = &["features", "backends"];

/// The frontend root of a checkout.
pub fn web_directory(repo_root: &Path) -> PathBuf {
  repo_root.join(WEB_DIR_NAME)
}

/// The web app's template directory.
pub fn web_templates_directory(repo_root: &Path) -> PathBuf {
  web_directory(repo_root).join(TEMPLATES_DIR_NAME)
}

/// The web app's static file directory.
pub fn web_static_directory(repo_root: &Path) -> PathBuf {
  web_directory(repo_root).join(STATIC_DIR_NAME)
}

/// The directory of a dotted page package: `features.<name>` maps to `web/features/<name>` and `backends.<name>` to
/// `web/backends/<name>` under `repo_root`. Any other first part (the `runtime.*` registrations, whose directories no
/// checkout carries) maps under `repo_root` unchanged.
pub fn package_directory(repo_root: &Path, package: &str) -> PathBuf {
  let under_web =
    WEB_PACKAGE_KINDS.iter().any(|kind| package.strip_prefix(kind).is_some_and(|rest| rest.starts_with('.')));
  let root = if under_web { web_directory(repo_root) } else { repo_root.to_path_buf() };
  package.split('.').fold(root, |path, part| path.join(part))
}

/// The template directory of one dotted page package.
pub fn package_templates_directory(repo_root: &Path, package: &str) -> PathBuf {
  package_directory(repo_root, package).join(TEMPLATES_DIR_NAME)
}

/// The static file directory of one dotted page package.
pub fn package_static_directory(repo_root: &Path, package: &str) -> PathBuf {
  package_directory(repo_root, package).join(STATIC_DIR_NAME)
}

/// Whether a changed file, named by its path relative to the repository root with `/` separators, is one the server
/// serves or renders. Those are the files under the web app's `templates/` and `static/` (`web_templates_directory`,
/// `web_static_directory`) and under the `templates/` and `static/` of each page package
/// (`package_templates_directory`, `package_static_directory`), where a page package is one dotted part below its kind
/// in `WEB_PACKAGE_KINDS`. Any other path under the frontend root, such as a test, the package manifest or a package's
/// own script, is not served.
pub fn is_served_path(path: &str) -> bool {
  let Some(under_web) = path.strip_prefix(WEB_DIR_NAME).and_then(|rest| rest.strip_prefix('/')) else {
    return false;
  };
  let parts: Vec<&str> = under_web.split('/').collect();
  match parts.as_slice() {
    [dir, _, ..] if is_served_dir(dir) => true,
    [kind, _, dir, _, ..] => WEB_PACKAGE_KINDS.contains(kind) && is_served_dir(dir),
    _ => false,
  }
}

/// Whether a directory name is one the server serves files from: `templates` or `static`.
fn is_served_dir(name: &str) -> bool {
  name == TEMPLATES_DIR_NAME || name == STATIC_DIR_NAME
}

/// The URL prefix of the static route: `web/static` and each page package's `static/` are served below it.
///
/// The router mounts it, the access-key gate leaves it public, and the template engine inlines a stylesheet whose
/// link is below it.
pub const STATIC_URL_PREFIX: &str = "/static/";

/// [`STATIC_URL_PREFIX`] without its trailing slash, the form a mount's prefix stores.
pub fn static_mount_prefix() -> &'static str {
  STATIC_URL_PREFIX.trim_end_matches('/')
}

/// The URL prefix of one dotted page package's `static/` directory, mirroring [`package_directory`]: `features.<name>`
/// maps to `/static/features/<name>`.
pub fn package_static_prefix(package: &str) -> String {
  format!("{STATIC_URL_PREFIX}{}", package.replace('.', "/"))
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
        package_templates_directory(repo_root, package).join(&name).is_file().then_some(name)
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
    let packages = ["features.zeta", "features.alpha"];
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
