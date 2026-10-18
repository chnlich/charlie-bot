//! Page hooks (`src/runtime/hooks/page_render.py`): page packages and their page parts, template globals and the
//! home-page cards.
//!
//! A page package keeps its page parts at fixed paths inside its directory `src/<kind>/<name>/`:
//! `static/` serves at `/static/<kind>/<name>/`, `templates/` joins the template search path, and
//! `templates/<name>/page_<slot>.html` is included where a page places the slot.

use std::path::{Path, PathBuf};

use charliebot_core::pyfmt;

use crate::context::FeatureContext;
use crate::registry::{Registry, RegistryError};

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

/// A zero-argument callable a template calls at render (`page_render.register_template_global`). The function sees
/// the live config and credentials through the feature context. An `Err` fails the render with its message.
pub type TemplateGlobalFn = fn(&FeatureContext) -> Result<minijinja::Value, String>;

/// One `/home` page card (`page_render.register_home_card`). Cards list in registration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HomeCard {
  pub name: &'static str,
  pub url: &'static str,
  pub description: &'static str,
}

/// `src/<kind>/<name>` for the package `src.<kind>.<name>`, under the checkout at `repo_root`.
pub fn package_directory(repo_root: &Path, package: &str) -> PathBuf {
  package.split('.').fold(repo_root.to_path_buf(), |path, part| path.join(part))
}

/// The slot's part templates that exist on disk now, as `<name>/page_<slot>.html`, in package order.
///
/// `packages` is the registry's [`Registry::page_packages`]. An unknown slot fails.
pub fn page_parts(repo_root: &Path, packages: &[&str], slot: &str) -> Result<Vec<String>, String> {
  if !PAGE_SLOTS.contains(&slot) {
    let slots = PAGE_SLOTS.iter().map(|name| pyfmt::str_repr(name)).collect::<Vec<_>>().join(", ");
    return Err(format!("page slot {} is not one of ({slots})", pyfmt::str_repr(slot)));
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

impl Registry {
  /// Register a template global (`page_render.register_template_global`).
  pub fn template_global(&mut self, name: &'static str, global: TemplateGlobalFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.template_globals.insert(package, name, global)
  }

  /// Register a home-page card (`page_render.register_home_card`).
  pub fn home_card(&mut self, card: HomeCard) -> Result<(), RegistryError> {
    self.owner()?;
    self.home_cards.push(card);
    Ok(())
  }

  /// The registered packages in registration order: the template search path and the static mounts follow it.
  pub fn page_packages(&self) -> &[&'static str] {
    &self.page_packages
  }

  /// The template globals with their names, in registration order.
  pub fn template_globals(&self) -> impl Iterator<Item = (&'static str, TemplateGlobalFn)> + '_ {
    self.template_globals.pairs().map(|(name, global)| (name, *global))
  }

  /// The home-page cards in registration order.
  pub fn home_cards(&self) -> &[HomeCard] {
    &self.home_cards
  }
}
