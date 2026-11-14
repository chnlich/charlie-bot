//! The `storage_cool_sweep_done` event of one real sweep, read through the shared test capture.

use std::cell::RefCell;
use std::fs;
use std::sync::Arc;

use charliebot_claude_code as claude_code;
use charliebot_claude_code::ClaudeHost;
use charliebot_claude_code::usage_quota::QuotaHttp;
use charliebot_codex as codex;
use charliebot_codex::CodexHost;
use charliebot_core::paths::realpath;
use charliebot_core::test_support::RecordedEvents;
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::UtcDateTime;
use charliebot_usage_page::cool_storage::run_cool_sweep;

thread_local! {
  /// The hosts the test registers its sources with. `register_package` takes a plain function, so the test hands them over here.
  static HOSTS: RefCell<Option<(ClaudeHost, CodexHost)>> = const { RefCell::new(None) };
}

fn register_sources(registry: &mut Registry) -> Result<(), RegistryError> {
  let (claude, codex_host) = HOSTS.with(|slot| slot.borrow_mut().take()).expect("the test sets its hosts first");
  let http = QuotaHttp::from_process().expect("the test HTTP client builds");
  claude_code::register_with_host(registry, claude, http)?;
  codex::register_with_host(registry, &codex_host)
}

#[test]
fn sweep_done_event_carries_each_field() {
  let tree = tempfile::tempdir().expect("the temporary tree");
  let charlie = tree.path().join("charlie");
  let home = tree.path().join("home");
  fs::create_dir_all(&charlie).expect("the CharlieBot home");
  let worktrees = tree.path().join("worktrees");
  let claude_config = tree.path().join("claude-config");
  fs::write(charlie.join("config.yaml"), format!("paths:\n  worktree_dir: '{}'\n", worktrees.display()))
    .expect("the synthetic config");
  // The sandbox guard: the home and every store the sweep reads resolve inside the temporary tree.
  let codex_home = home.join(".codex");
  for path in [&charlie, &home, &claude_config, &worktrees, &codex_home] {
    assert!(
      realpath(path).starts_with(realpath(tree.path())),
      "{} resolves outside the synthetic tree",
      path.display()
    );
  }
  HOSTS.with(|slot| {
    *slot.borrow_mut() = Some((
      ClaudeHost { home: home.clone(), claude_config_dir: claude_config },
      CodexHost { path: "/usr/bin:/bin".to_string(), home },
    ))
  });
  let mut registry = Registry::new();
  registry.register_package("usage-cool-storage-log", register_sources).expect("the test sources register");
  let cfg = Arc::new(registry.load_config(&charlie).expect("the synthetic config loads"));
  let now = UtcDateTime::parse("2026-09-04T12:00:00+00:00").expect("the instant parses");
  let recorded = RecordedEvents::default();
  let _capture = charliebot_core::test_support::install_events(Arc::new(recorded.clone()));
  run_cool_sweep(Arc::clone(&cfg), &registry, now).expect("the sweep runs");

  let events = recorded.events();
  let done = events
    .iter()
    .find(|fields| fields.get("message").is_some_and(|text| text == "storage_cool_sweep_done"))
    .expect("the sweep logs its summary");
  let mut names: Vec<&str> = done.keys().map(String::as_str).filter(|name| *name != "message").collect();
  names.sort_unstable();
  // The sweep summary logs dry_run, session_id and total_bytes, then one field per category.
  assert_eq!(names, ["claude_transcripts", "codex_rollouts", "dry_run", "raw_transport", "session_id", "total_bytes"]);
  assert_eq!(done["session_id"], "None");
  assert_eq!(done["dry_run"], "false");
}
