//! The memory package's turn contribution (`src/features/memory/turn_contribution.py`): the applicable memory joins a
//! run's managed instructions.

use anyhow::Result;
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::paths;
use charliebot_registry::turn::{PromptSource, RuleSegment, TurnContribution};
use charliebot_store::metadata::SessionMetadata;

use crate::memory::{self, MemorySelection};
use crate::store_root;

/// The run kinds whose managed instructions are the manager contract (`task_prompts.MANAGER_KINDS`). Every other kind
/// takes the worker audience.
const MANAGER_KINDS: &[&str] = &["manager_turn"];

/// The scope of a memory source (`task_prompts.SCOPE_MEMORY`).
const SCOPE_MEMORY: &str = "memory";

/// The memory package's turn contribution (`turn_contribution.MemoryTurnContribution`).
pub struct MemoryTurn;

impl TurnContribution for MemoryTurn {
  fn class_name(&self) -> &'static str {
    "MemoryTurnContribution"
  }

  fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    cfg: &CharlieBotConfig,
    _repo: &RepoRoot,
  ) -> Result<Vec<RuleSegment>> {
    let selection = memory_selection_for(meta, kind, cfg)?;
    Ok(selection.map(|selection| rule_segments(&selection)).unwrap_or_default())
  }
}

/// The audience mapping (`turn_contribution.memory_selection_for`): a manager takes the master selection, and every
/// other kind the worker selection for its repo.
fn memory_selection_for(meta: &SessionMetadata, kind: &str, cfg: &CharlieBotConfig) -> Result<Option<MemorySelection>> {
  let memory_dir = store_root::memory_dir(cfg);
  if MANAGER_KINDS.contains(&kind) {
    return memory::select_master_memory(&memory_dir);
  }
  Ok(Some(memory::select_worker_memory(&memory_dir, repo_basename(meta))?))
}

/// The basename of the task's repo, or `""` for a task with no repo: a repo-less worker matches no repo topic.
fn repo_basename(meta: &SessionMetadata) -> &str {
  match meta.task.as_ref().and_then(|task| task.repo_path.as_deref()) {
    Some(repo_path) if !repo_path.is_empty() => paths::name(repo_path),
    _ => "",
  }
}

/// The selection's segments as ordered rule segments, each source with scope `memory`
/// (`turn_contribution._memory_rule_segments`).
fn rule_segments(selection: &MemorySelection) -> Vec<RuleSegment> {
  selection
    .segments
    .iter()
    .map(|segment| RuleSegment {
      text: segment.text.clone(),
      sources: segment
        .sources
        .iter()
        .map(|source| PromptSource {
          scope: SCOPE_MEMORY.to_string(),
          source_ref: source.source_ref.clone(),
          source_session_id: None,
        })
        .collect(),
      delivery: segment.delivery.to_string(),
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::Path;

  use charliebot_core::RepoRoot;
  use charliebot_core::config::{
    BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
  };
  use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec};

  use super::*;

  /// The checkout the memory segments never read: the contribution takes it and ignores it.
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
        session_tasks_max: 0,
        min_free_disk_gib: 0,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    }
  }

  fn worker_meta(repo_path: Option<&str>) -> SessionMetadata {
    let mut meta = SessionMetadata::new("synthetic-worker", "worker", TaskProfile::Worker);
    meta.task = Some(TaskSpec {
      goal: "synthetic goal".to_string(),
      repo_path: repo_path.map(str::to_string),
      ..Default::default()
    });
    meta
  }

  /// A home whose store has a master-resident note and a worker note in topic `gamma-service`.
  fn seeded_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join("memory");
    fs::create_dir_all(store.join("entries/profile")).unwrap();
    fs::create_dir_all(store.join("entries/gamma-service")).unwrap();
    fs::write(store.join("topics"), "profile resident\ngamma-service\n").unwrap();
    fs::write(
      store.join("entries/profile/resident-note.md"),
      "---\nscope: user\ntopic: profile\naudience: master, worker\ntitle: Resident Note\n---\nresident memory\n",
    )
    .unwrap();
    fs::write(
      store.join("entries/gamma-service/build.md"),
      "---\nscope: user\ntopic: gamma-service\naudience: worker\ntitle: Build\n---\nbuild steps\n",
    )
    .unwrap();
    home
  }

  #[test]
  fn a_manager_gets_the_resident_entry_as_a_memory_source() {
    let home = seeded_home();
    let segments = MemoryTurn
      .instruction_segments(
        &SessionMetadata::new("s", "s", TaskProfile::Manager),
        "manager_turn",
        &config(home.path()),
        &checkout(),
      )
      .unwrap();
    let full = segments.iter().find(|segment| segment.delivery == "full").expect("a full segment");
    assert_eq!(full.sources[0].scope, SCOPE_MEMORY);
    assert_eq!(full.sources[0].source_ref, "memory:profile/resident-note");
    assert!(full.text.contains("resident memory"));
  }

  #[test]
  fn a_worker_gets_its_repo_topic_in_full_and_no_other_repo_in_full() {
    let home = seeded_home();
    let cfg = config(home.path());
    let matched = worker_meta(Some("/home/synthetic/gamma-service/"));
    let segments = MemoryTurn.instruction_segments(&matched, "work", &cfg, &checkout()).unwrap();
    let full = segments.iter().find(|segment| segment.delivery == "full").expect("a full segment");
    assert_eq!(full.sources[0].source_ref, "memory:gamma-service/build");

    let other = worker_meta(Some("/home/synthetic/unrelated-repo"));
    let segments = MemoryTurn.instruction_segments(&other, "work", &cfg, &checkout()).unwrap();
    assert!(segments.iter().all(|segment| segment.delivery != "full"));
  }

  #[test]
  fn a_repo_less_worker_gets_the_index_only_and_no_guessed_project() {
    let home = seeded_home();
    let cfg = config(home.path());
    let segments = MemoryTurn.instruction_segments(&worker_meta(None), "review", &cfg, &checkout()).unwrap();
    assert!(segments.iter().all(|segment| segment.delivery != "full"));
    assert!(segments.iter().any(|segment| segment.delivery == "index"));
  }
}
