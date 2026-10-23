//! The startup sweep's scope and its `session_cgroup_sweep_removed` line, read through the shared test capture.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use charliebot_agents::transport::cgroup::sweep_stale_session_cgroups;
use charliebot_core::test_support::EventSink;
use tracing::Event;
use tracing::field::{Field, Visit};

const SESSION_ID: &str = "abcd1234-ef56-7890-abcd-ef1234567890";
const POPULATED_ID: &str = "9f8e7d6c-0000-4000-8000-000000000000";

/// Each event the capture records: each field's name and Debug text.
#[derive(Clone, Default)]
struct RecordedEvents(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

impl EventSink for RecordedEvents {
  fn event(&self, event: &Event<'_>) {
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
fn sweep_removes_only_empty_cgroups_named_for_this_homes_sessions() {
  let base = tempfile::tempdir().unwrap();
  let home = tempfile::tempdir().unwrap();
  let sessions = home.path().join("sessions");
  fs::create_dir(&sessions).unwrap();
  fs::create_dir(sessions.join(SESSION_ID)).unwrap();
  fs::create_dir(sessions.join(POPULATED_ID)).unwrap();
  fs::create_dir(base.path().join("charliebot-sess-abcd1234")).unwrap();
  fs::create_dir(base.path().join("charliebot-sess-0badc0de")).unwrap();
  let populated = base.path().join("charliebot-sess-9f8e7d6c");
  fs::create_dir(&populated).unwrap();
  fs::write(populated.join("cgroup.procs"), "1").unwrap();
  fs::create_dir(base.path().join("app-other.scope")).unwrap();

  let recorded = RecordedEvents::default();
  let _capture = charliebot_core::test_support::install_events(Arc::new(recorded.clone()));
  let removed = sweep_stale_session_cgroups(base.path(), &sessions).expect("the home sessions directory can be read");

  assert_eq!(removed, 1);
  assert!(!base.path().join("charliebot-sess-abcd1234").exists());
  assert!(base.path().join("charliebot-sess-0badc0de").is_dir());
  assert!(populated.is_dir());
  assert!(base.path().join("app-other.scope").is_dir());
  let events = recorded.0.lock().expect("the recorder lock");
  let sweeps: Vec<&BTreeMap<String, String>> = events
    .iter()
    .filter(|fields| fields.get("message").is_some_and(|text| text == "session_cgroup_sweep_removed"))
    .collect();
  assert_eq!(sweeps.len(), 1);
  assert_eq!(sweeps[0]["count"], "1");
  assert_eq!(sweeps[0]["names"], r#"["charliebot-sess-abcd1234"]"#);
}

#[test]
fn sweep_propagates_a_missing_sessions_directory() {
  let base = tempfile::tempdir().unwrap();
  let cgroup = base.path().join("charliebot-sess-abcd1234");
  fs::create_dir(&cgroup).unwrap();

  let error = sweep_stale_session_cgroups(base.path(), &base.path().join("missing-sessions"))
    .expect_err("a missing home sessions directory fails the sweep");

  assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
  assert!(cgroup.is_dir());
}
