//! The `storage_cool_sweep_done` event of one real sweep, read through a tracing subscriber. It runs in its own test binary:
//! a callsite that an earlier test of the same process reached with no subscriber keeps an uninteresting cache entry, and the
//! event would never reach this subscriber.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use charliebot_agents::backends::claude_code::usage_quota::QuotaHttp;
use charliebot_agents::backends::claude_code::{self, ClaudeHost};
use charliebot_agents::backends::codex::{self, CodexHost};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::UtcDateTime;
use charliebot_usage_page::cool_storage::run_cool_sweep;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

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

/// Each event the subscriber records: its field names, and each field's Debug text.
#[derive(Clone, Default)]
struct RecordedEvents(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

impl<S: Subscriber> Layer<S> for RecordedEvents {
  fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
    let mut fields = BTreeMap::new();
    event.record(&mut FieldText(&mut fields));
    self.0.lock().expect("the recorder lock").push(fields);
  }
}

struct FieldText<'a>(&'a mut BTreeMap<String, String>);

impl Visit for FieldText<'_> {
  fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
    self.0.insert(field.name().to_string(), format!("{value:?}"));
  }
}

#[test]
fn sweep_done_event_carries_each_python_field() {
  let tree = tempfile::tempdir().expect("the temporary tree");
  let charlie = tree.path().join("charlie");
  let home = tree.path().join("home");
  fs::create_dir_all(&charlie).expect("the CharlieBot home");
  let worktrees = tree.path().join("worktrees");
  fs::write(charlie.join("config.yaml"), format!("paths:\n  worktree_dir: '{}'\n", worktrees.display()))
    .expect("the synthetic config");
  HOSTS.with(|slot| {
    *slot.borrow_mut() = Some((
      ClaudeHost { home: home.clone(), claude_config_dir: tree.path().join("claude-config") },
      CodexHost { path: "/usr/bin:/bin".to_string(), home },
    ))
  });
  let mut registry = Registry::new();
  registry.register_package("usage-cool-storage-log", register_sources).expect("the test sources register");
  let cfg = Arc::new(registry.load_config(&charlie).expect("the synthetic config loads"));
  let now = UtcDateTime::parse("2026-09-04T12:00:00+00:00").expect("the instant parses");
  let recorded = RecordedEvents::default();
  let subscriber = tracing_subscriber::Registry::default().with(recorded.clone());

  tracing::subscriber::with_default(subscriber, || {
    run_cool_sweep(Arc::clone(&cfg), &registry, now).expect("the sweep runs");
  });

  let events = recorded.0.lock().expect("the recorder lock");
  let done = events
    .iter()
    .find(|fields| fields.get("message").is_some_and(|text| text == "storage_cool_sweep_done"))
    .expect("the sweep logs its summary");
  let mut names: Vec<&str> = done.keys().map(String::as_str).filter(|name| *name != "message").collect();
  names.sort_unstable();
  // storage_cool.py:305-311 passes dry_run, session_id and total_bytes, then one keyword per category.
  assert_eq!(names, ["claude_transcripts", "codex_rollouts", "dry_run", "raw_transport", "session_id", "total_bytes"]);
  assert_eq!(done["session_id"], "None");
  assert_eq!(done["dry_run"], "false");
}
