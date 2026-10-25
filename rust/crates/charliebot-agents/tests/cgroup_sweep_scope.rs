//! The startup sweep's scope and its `session_cgroup_sweep_removed` line, read through the shared test capture.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use charliebot_agents::transport::cgroup::sweep_stale_session_cgroups;
use charliebot_core::cgroup::{RETIRED_APP_SLICE, ResourceClass, session_slice_dir, user_slice_dir};
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
fn sweep_removes_only_empty_cgroups_of_this_home_across_the_slices() {
  let base = tempfile::tempdir().unwrap();
  let home = tempfile::tempdir().unwrap();
  let sessions = home.path().join("sessions");
  fs::create_dir(&sessions).unwrap();
  fs::create_dir(sessions.join(SESSION_ID)).unwrap();
  fs::create_dir(sessions.join(POPULATED_ID)).unwrap();
  let agents_slice = session_slice_dir(base.path(), ResourceClass::Agent);
  let repair_slice = session_slice_dir(base.path(), ResourceClass::Repair);
  let retired_slice = user_slice_dir(base.path(), RETIRED_APP_SLICE);
  for slice in [&agents_slice, &repair_slice, &retired_slice] {
    fs::create_dir_all(slice).unwrap();
  }
  // The same home's cgroup in each slice; the retired slice's leftover is cleared too.
  let agents_empty = agents_slice.join("charliebot-sess-abcd1234");
  let repair_empty = repair_slice.join("charliebot-sess-abcd1234");
  let retired_empty = retired_slice.join("charliebot-sess-abcd1234");
  for path in [&agents_empty, &repair_empty, &retired_empty] {
    fs::create_dir(path).unwrap();
  }
  let populated = agents_slice.join("charliebot-sess-9f8e7d6c");
  fs::create_dir(&populated).unwrap();
  fs::write(populated.join("cgroup.procs"), "1").unwrap();
  let other_home = repair_slice.join("charliebot-sess-0badc0de");
  fs::create_dir(&other_home).unwrap();
  let non_session = agents_slice.join("app-other.scope");
  fs::create_dir(&non_session).unwrap();

  let recorded = RecordedEvents::default();
  let _capture = charliebot_core::test_support::install_events(Arc::new(recorded.clone()));
  let removed = sweep_stale_session_cgroups(base.path(), &sessions).expect("the home sessions directory can be read");

  assert_eq!(removed, 3);
  assert!(!agents_empty.exists());
  assert!(!repair_empty.exists());
  assert!(!retired_empty.exists());
  assert!(populated.is_dir());
  assert!(other_home.is_dir());
  assert!(non_session.is_dir());
  let events = recorded.0.lock().expect("the recorder lock");
  let sweeps: Vec<&BTreeMap<String, String>> = events
    .iter()
    .filter(|fields| fields.get("message").is_some_and(|text| text == "session_cgroup_sweep_removed"))
    .collect();
  assert_eq!(sweeps.len(), 1);
  assert_eq!(sweeps[0]["count"], "3");
  assert_eq!(
    sweeps[0]["names"],
    r#"["charliebot-sess-abcd1234", "charliebot-sess-abcd1234", "charliebot-sess-abcd1234"]"#
  );
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
