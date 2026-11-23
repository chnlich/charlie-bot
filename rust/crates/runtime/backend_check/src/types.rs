//! The scan's public types: the roots that a scan reads under, and the hits that it returns.
//!
//! `api` builds a scan from these, and `gate` reports the hits.

use std::path::PathBuf;

use crate::exemptions::Exemption;
use crate::rules::Class;

/// The roots that an exemption's path is read under: the repository root, and the profile home.
pub struct Roots {
  pub repo: Option<PathBuf>,
  pub home: PathBuf,
}

/// One site that a class matches. `exempt` names the row that keeps the site, or is `None` for a site to fix.
#[derive(Debug)]
pub struct Hit {
  pub path: PathBuf,
  pub line: usize,
  pub class: Class,
  pub value: String,
  pub exempt: Option<&'static Exemption>,
}
