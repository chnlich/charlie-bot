//! Structured logging: one line per event on stdout, in the grammar `logline` writes and reads.
//!
//! An event's message is its name, the same name the Python server's structlog call logs
//! (`tracing::info!(path = ..., "charliebot_home_ready")`). A field named `event` names the event instead, and does not
//! repeat after it; with both, the `event` field wins, as the JSON writer this format replaced extended the message's
//! map with the fields. A line reads `<UTC time> [<level>] <location> <event> <key=value>...`, with the fields in call
//! order and the location the file and line of the tracing call.

use std::io::Write;

use serde_json::Value;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

use crate::{logline, timefmt};

/// Install the line layer as the process's global subscriber: CharlieBot's own events from INFO, library events (axum,
/// hyper, tower) from WARN.
pub fn init() {
  let filter = targets_filter();
  let subscriber = Registry::default().with(StdoutLines.with_filter(filter));
  tracing::subscriber::set_global_default(subscriber).expect("logging initializes once, before any event");
}

fn targets_filter() -> Targets {
  Targets::new().with_target("charliebot", Level::INFO).with_default(Level::WARN)
}

/// Writes each event as one flushed line on stdout.
struct StdoutLines;

impl<S: Subscriber> Layer<S> for StdoutLines {
  fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
    let line = render_line(event);
    let mut stdout = std::io::stdout().lock();
    // A closed stdout leaves the server nowhere to report to; the event is dropped with it.
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
  }
}

/// The line one event renders to. `on_event` writes it, and the release-helper tests read real lines back through it.
pub fn render_line(event: &Event<'_>) -> String {
  let mut fields = FieldMap::default();
  event.record(&mut fields);
  let metadata = event.metadata();
  let level = logline::level_word(metadata.level());
  let name = fields.event.or(fields.message).unwrap_or_default();
  let file = metadata.file().expect("tracing macros record the file");
  let line = metadata.line().expect("tracing macros record the line");
  logline::render(&timefmt::log_timestamp(), level, &location_text(file, line), &name, &fields.fields)
}

/// The location column's text: the file and line of the tracing call, as `<path>:<line>` with the path shortened.
pub fn location_text(file: &str, line: u32) -> String {
  format!("{}:{line}", shortened_file(file))
}

/// A tracing file path as the location column reads it: a workspace path loses its group and `src/`, a cargo registry
/// path keeps its package and everything after it, and any other path stays as the compiler gave it.
fn shortened_file(file: &str) -> String {
  if let Some(shortened) = workspace_file(file) {
    return shortened;
  }
  if let Some(shortened) = registry_file(file) {
    return shortened;
  }
  file.to_string()
}

/// `crates/<group>/<crate>/src/<rest>` as `<crate>/<rest>`.
fn workspace_file(file: &str) -> Option<String> {
  let segments: Vec<&str> = file.split('/').collect();
  if segments.len() < 5 || segments[0] != "crates" || segments[3] != "src" {
    return None;
  }
  Some(format!("{}/{}", segments[2], segments[4..].join("/")))
}

/// `…/registry/src/<index>/<package>-<version>/<rest>` as `<package>-<version>/<rest>`.
fn registry_file(file: &str) -> Option<String> {
  let (_, after) = file.split_once("registry/src/")?;
  let (_, after) = after.split_once('/')?;
  let (package, rest) = after.split_once('/')?;
  Some(format!("{package}/{rest}"))
}

/// An event's fields: `message` and `event` name the line, the rest keep their call order.
#[derive(Default)]
struct FieldMap {
  message: Option<String>,
  event: Option<String>,
  fields: Vec<(String, Value)>,
}

impl FieldMap {
  fn put(&mut self, field: &Field, value: Value) {
    match field.name() {
      "message" => self.message = Some(logline::value_text(&value)),
      "event" => self.event = Some(logline::value_text(&value)),
      _ => self.fields.push((field.name().to_string(), value)),
    }
  }
}

impl Visit for FieldMap {
  fn record_i64(&mut self, field: &Field, value: i64) {
    self.put(field, Value::from(value));
  }

  fn record_u64(&mut self, field: &Field, value: u64) {
    self.put(field, Value::from(value));
  }

  fn record_f64(&mut self, field: &Field, value: f64) {
    self.put(field, Value::from(value));
  }

  fn record_bool(&mut self, field: &Field, value: bool) {
    self.put(field, Value::from(value));
  }

  fn record_str(&mut self, field: &Field, value: &str) {
    self.put(field, Value::from(value));
  }

  fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
    self.put(field, Value::from(format!("{value:?}")));
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};

  use tracing::{Event, Subscriber};
  use tracing_subscriber::layer::{Context, SubscriberExt};
  use tracing_subscriber::{Layer, Registry};

  use super::{location_text, render_line, targets_filter};
  use crate::logline;

  struct Capture(Arc<Mutex<Vec<(String, String)>>>);

  impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
      self.0.lock().unwrap().push((event.metadata().target().to_string(), event.metadata().level().to_string()));
    }
  }

  struct Rendered(Arc<Mutex<Vec<String>>>);

  impl<S: Subscriber> Layer<S> for Rendered {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
      self.0.lock().unwrap().push(render_line(event));
    }
  }

  /// The lines the calls in `recording` render.
  fn rendered(recording: impl FnOnce()) -> Vec<String> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let subscriber = Registry::default().with(Rendered(Arc::clone(&lines)));
    tracing::subscriber::with_default(subscriber, recording);
    lines.lock().unwrap().clone()
  }

  #[test]
  fn target_filter_covers_charliebot_prefix_and_keeps_third_party_at_warn() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = Registry::default().with(Capture(Arc::clone(&events)).with_filter(targets_filter()));
    tracing::subscriber::with_default(subscriber, || {
      tracing::info!(target: "charliebot_tasks", "agent event");
      tracing::info!(target: "third_party", "filtered library event");
      tracing::warn!(target: "third_party", "visible library warning");
    });
    assert_eq!(
      *events.lock().unwrap(),
      [("charliebot_tasks".to_string(), "INFO".to_string()), ("third_party".to_string(), "WARN".to_string()),]
    );
  }

  #[test]
  fn an_event_field_names_the_line_and_does_not_repeat_as_a_field() {
    let lines = rendered(|| tracing::info!(event = "slack_listener_connected"));
    let [line] = lines.as_slice() else { panic!("one call rendered {:?}", lines) };
    assert!(line.ends_with("slack_listener_connected"), "{line}");
    assert!(!line.contains("event="), "{line}");
    let row = logline::parse(line).expect("the line reads back");
    assert_eq!(row.event, "slack_listener_connected");
    assert_eq!(row.field("event"), None);
  }

  #[test]
  fn a_call_renders_its_file_and_line_in_the_location_column() {
    use std::cell::Cell;

    let call_line = Cell::new(0);
    let lines = rendered(|| {
      call_line.replace(line!() + 1);
      tracing::info!(event = "slack_listener_connected");
    });
    let [line] = lines.as_slice() else { panic!("one call rendered {:?}", lines) };
    let row = logline::parse(line).expect("the line reads back");
    assert_eq!(row.location, format!("core/logging.rs:{}", call_line.get()), "{line}");
    assert_eq!(row.event, "slack_listener_connected");
  }

  #[test]
  fn a_workspace_path_drops_the_group_and_src() {
    assert_eq!(location_text("crates/features/slack/src/slack_listener.rs", 245), "slack/slack_listener.rs:245");
    assert_eq!(location_text("crates/infra/core/src/logging.rs", 50), "core/logging.rs:50");
    assert_eq!(
      location_text("crates/app/cli/src/commands/release_helper.rs", 100),
      "cli/commands/release_helper.rs:100"
    );
  }

  #[test]
  fn a_registry_path_keeps_the_package_and_the_rest() {
    assert_eq!(
      location_text("/home/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde_json-1.0.145/src/lib.rs", 7),
      "serde_json-1.0.145/src/lib.rs:7"
    );
  }

  #[test]
  fn any_other_path_is_written_as_the_compiler_gave_it() {
    assert_eq!(location_text("src/main.rs", 3), "src/main.rs:3");
    assert_eq!(location_text("crates/features/slack/build.rs", 9), "crates/features/slack/build.rs:9");
  }

  #[test]
  fn an_event_field_keeps_the_other_fields_in_call_order() {
    let lines = rendered(|| tracing::info!(event = "slack_listener_connected", path = "/slack/events", count = 2));
    let [line] = lines.as_slice() else { panic!("one call rendered {:?}", lines) };
    let row = logline::parse(line).expect("the line reads back");
    assert_eq!(row.event, "slack_listener_connected");
    assert_eq!(row.fields, [("path".to_string(), "/slack/events".to_string()), ("count".to_string(), "2".to_string())]);
  }

  #[test]
  fn an_event_field_wins_over_the_message_as_the_json_writer_did() {
    let lines = rendered(|| tracing::info!(event = "slack_listener_connected", "listener connected"));
    let [line] = lines.as_slice() else { panic!("one call rendered {:?}", lines) };
    let row = logline::parse(line).expect("the line reads back");
    assert_eq!(row.event, "slack_listener_connected");
    assert_eq!(row.fields, []);
  }

  #[test]
  fn a_message_call_still_names_the_event_from_its_message() {
    let lines = rendered(|| tracing::info!(path = "/api/internal/version", "charliebot_home_ready"));
    let [line] = lines.as_slice() else { panic!("one call rendered {:?}", lines) };
    let row = logline::parse(line).expect("the line reads back");
    assert_eq!(row.event, "charliebot_home_ready");
    assert_eq!(row.field("path"), Some("/api/internal/version"));
  }
}
