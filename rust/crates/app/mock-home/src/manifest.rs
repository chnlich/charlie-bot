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
  /// The `web_status` goal of a search match session and of the newest rows of a default home, which a row carries; the
  /// other sessions write none.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub web_status: Option<Value>,
}

/// The home's `manifest.json`: the seed, count, shape and language that produced it, each session's log size and newest
/// message in the order `generate` wrote the sessions, and the scheduled tasks of `.cron.d`. A default home's manifest
/// has no `shape` or `language` key, so its bytes stay those of the data version. The views write no tasks, so they have
/// no `cron_tasks` key.
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
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub cron_tasks: Vec<ManifestCronTask>,
}

#[derive(Serialize, Deserialize)]
pub struct ManifestSession {
  pub id: String,
  pub event_log_bytes: u64,
  /// The checkpoint that the session's newest chat message holds. A worker's transcript is built from its run files,
  /// and a worker without a run serves no message, so a worker's entry holds `None` and the check does not read it.
  pub last_message: Option<String>,
}

/// One scheduled task of `.cron.d`, and the manager session that its file binds.
#[derive(Serialize, Deserialize)]
pub struct ManifestCronTask {
  pub name: String,
  pub session_id: String,
}

impl Manifest {
  /// Reads the manifest that `generate` wrote into `home`.
  pub fn read(home: &Path) -> Result<Self> {
    let path = home.join(MANIFEST_NAME);
    let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not a manifest", path.display()))
  }
}
