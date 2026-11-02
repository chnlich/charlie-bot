//! The taste package's turn contribution: the precedent table's index joins every manager and worker run.
//!
//! The segment names each domain that holds an active row and carries no count or date, so adding a row to an
//! existing domain leaves its bytes unchanged. The index enters the hashed prompt blocks, and a changed hash starts a
//! fresh native conversation for every open manager.

use anyhow::Result;
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::turn::{PromptSource, RuleSegment, TurnContribution};
use charliebot_store::metadata::SessionMetadata;

use crate::store::{self, taste_dir};

/// The scope a taste segment's source carries.
const SCOPE_TASTE: &str = "taste";
/// The source reference of the whole index segment.
const INDEX_SOURCE_REF: &str = "taste:index";

/// The taste package's turn contribution.
pub struct TasteTurn;

impl TurnContribution for TasteTurn {
  fn class_name(&self) -> &'static str {
    "TasteTurnContribution"
  }

  /// The index segment for a run of any kind. A store with no active row adds nothing.
  fn instruction_segments(
    &self,
    _meta: &SessionMetadata,
    _kind: &str,
    cfg: &CharlieBotConfig,
    _repo: &RepoRoot,
  ) -> Result<Vec<RuleSegment>> {
    let text = store::load_store(&taste_dir(cfg))?.index_text();
    if text.is_empty() {
      return Ok(Vec::new());
    }
    Ok(vec![RuleSegment {
      text,
      sources: vec![PromptSource {
        scope: SCOPE_TASTE.to_string(),
        source_ref: INDEX_SOURCE_REF.to_string(),
        source_session_id: None,
      }],
      delivery: "index".to_string(),
    }])
  }
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use charliebot_core::config::{
    BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
  };
  use charliebot_registry::turn::TurnContribution;
  use serde_json::json;

  use super::*;

  fn checkout() -> RepoRoot {
    RepoRoot::new(Some("/synthetic/checkout".into())).unwrap()
  }

  fn config(home: &Path) -> CharlieBotConfig {
    CharlieBotConfig {
      charliebot_home: home.to_path_buf(),
      headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18650,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        public_url: None,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
        enable_session_cpu_weight: true,
        enable_session_io_weight: true,
        enable_local_work_guard: false,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig { pool: Vec::new(), routing: None, strong: None, options: Vec::new() },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    }
  }

  fn add(home: &Path, event_id: &str) {
    let row = json!({
      "type": "row",
      "decided_at": "2026-10-16T12:00:00+00:00",
      "domain": "work/code-review",
      "kind": "choice",
      "situation": "a plan fork",
      "options": ["a", "b"],
      "recommended": "a",
      "chosen": "b",
      "reason": "未说明",
      "source": {"session_id": "sess-1", "url": "/?session=sess-1", "event_id": event_id}
    });
    store::add_row(&home.join("taste"), &row).unwrap();
  }

  fn segment_text(home: &Path, kind: &str) -> Option<String> {
    let meta = SessionMetadata::new("synthetic", "synthetic", charliebot_store::metadata::TaskProfile::Manager);
    let segments = TasteTurn.instruction_segments(&meta, kind, &config(home), &checkout()).unwrap();
    segments.first().map(|segment| segment.text.clone())
  }

  #[test]
  fn an_empty_store_adds_no_segment() {
    let home = tempfile::tempdir().unwrap();
    assert!(
      TasteTurn
        .instruction_segments(
          &SessionMetadata::new("s", "s", charliebot_store::metadata::TaskProfile::Worker),
          "work",
          &config(home.path()),
          &checkout(),
        )
        .unwrap()
        .is_empty()
    );
  }

  #[test]
  fn manager_and_worker_runs_receive_the_same_index() {
    let home = tempfile::tempdir().unwrap();
    add(home.path(), "evt-1");
    let expected = store::load_store(&home.path().join("taste")).unwrap().index_text();
    assert_eq!(segment_text(home.path(), "manager_turn").as_deref(), Some(expected.as_str()));
    for worker_kind in ["work", "review", "verify", "cold-read"] {
      assert_eq!(segment_text(home.path(), worker_kind).as_deref(), Some(expected.as_str()), "{worker_kind}");
    }
    let meta = SessionMetadata::new("synthetic", "synthetic", charliebot_store::metadata::TaskProfile::Manager);
    let segment =
      TasteTurn.instruction_segments(&meta, "manager_turn", &config(home.path()), &checkout()).unwrap().remove(0);
    assert_eq!(segment.delivery, "index");
    assert_eq!(segment.sources[0].scope, "taste");
    assert_eq!(segment.sources[0].source_ref, "taste:index");
  }
}
