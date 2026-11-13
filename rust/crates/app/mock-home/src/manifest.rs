//! The shapes of `metadata.json` and `manifest.json`. `generate` writes both, and `check` reads the manifest back.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Language;
use crate::Shape;

pub const MANIFEST_NAME: &str = "manifest.json";

/// One session's `metadata.json`, with the fields in the order the server's session list reads them.
#[derive(Serialize)]
pub struct SessionMetadata {
  pub id: String,
  pub name: String,
  pub status: &'static str,
  pub profile: &'static str,
  pub backend: &'static str,
  pub created_at: String,
  pub updated_at: String,
  pub schema_version: u32,
  pub has_unread: bool,
  pub starred: bool,
  /// The task spec of a view session; the other shapes write none.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub task: Option<Value>,
}

/// The home's `manifest.json`: the seed, count, shape and language that produced it, and each session's log size and
/// newest message, in the order `generate` wrote the sessions. A default home's manifest has no `shape` or `language`
/// key, so its bytes stay those of the data version.
#[derive(Serialize, Deserialize)]
pub struct Manifest {
  pub seed: u64,
  pub session_count: usize,
  pub event_log_bytes: u64,
  pub sessions: Vec<ManifestSession>,
  #[serde(default, skip_serializing_if = "Shape::is_default")]
  pub shape: Shape,
  #[serde(default, skip_serializing_if = "Language::is_mixed")]
  pub language: Language,
}

#[derive(Serialize, Deserialize)]
pub struct ManifestSession {
  pub id: String,
  pub event_log_bytes: u64,
  pub last_message: String,
}

impl Manifest {
  /// Reads the manifest that `generate` wrote into `home`.
  pub fn read(home: &Path) -> Result<Self> {
    let path = home.join(MANIFEST_NAME);
    let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not a manifest", path.display()))
  }
}
