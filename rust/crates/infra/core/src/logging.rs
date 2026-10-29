//! Structured logging: one line per event on stdout, in the grammar `logline` writes and reads.
//!
//! An event's message is its name, the same name the Python server's structlog call logs
//! (`tracing::info!(path = ..., "charliebot_home_ready")`). A field named `event` names the event instead, and does not
//! repeat after it; with both, the `event` field wins, as the JSON writer this format replaced extended the message's
//! map with the fields. A line reads `<UTC time> [<level>] <event> <key=value>...`, with the fields in call order.

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
  let level = logline::level_word(event.metadata().level());
  let name = fields.event.or(fields.message).unwrap_or_default();
  logline::render(&timefmt::log_timestamp(), level, &name, &fields.fields)
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

  use super::{render_line, targets_filter};
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
