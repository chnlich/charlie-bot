//! Structured logging: one JSON line per event on stdout.
//!
//! An event's message is its name, the same name the Python server's structlog call logs
//! (`tracing::info!(path = ..., "charliebot_home_ready")`). A line reads
//! `{"event": <name>, <fields in call order>, "level": <level>, "timestamp": <ISO 8601 UTC>}`, the key
//! layout of structlog's JSON renderer with `add_log_level` and an ISO `TimeStamper`.

use std::io::Write;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

use crate::timefmt;

/// Install the JSON line layer as the process's global subscriber: CharlieBot's own events from INFO,
/// library events (axum, hyper, tower) from WARN.
pub fn init() {
  let filter = targets_filter();
  let subscriber = Registry::default().with(JsonLines.with_filter(filter));
  tracing::subscriber::set_global_default(subscriber).expect("logging initializes once, before any event");
}

fn targets_filter() -> Targets {
  Targets::new().with_target("charliebot", Level::INFO).with_default(Level::WARN)
}

/// Writes each event as one flushed JSON line on stdout.
struct JsonLines;

impl<S: Subscriber> Layer<S> for JsonLines {
  fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
    let line = render_line(event);
    let mut stdout = std::io::stdout().lock();
    // A closed stdout leaves the server nowhere to report to; the event is dropped with it.
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
  }
}

fn render_line(event: &Event<'_>) -> String {
  let mut fields = FieldMap { event: None, fields: Map::new() };
  event.record(&mut fields);
  let mut line = Map::new();
  line.insert("event".to_string(), Value::String(fields.event.unwrap_or_default()));
  line.extend(fields.fields);
  line.insert("level".to_string(), Value::String(level_name(event.metadata().level()).to_string()));
  line.insert("timestamp".to_string(), Value::String(timefmt::log_timestamp()));
  Value::Object(line).to_string()
}

fn level_name(level: &Level) -> &'static str {
  match *level {
    Level::ERROR => "error",
    Level::WARN => "warning",
    Level::INFO => "info",
    Level::DEBUG => "debug",
    Level::TRACE => "trace",
  }
}

struct FieldMap {
  event: Option<String>,
  fields: Map<String, Value>,
}

impl FieldMap {
  fn put(&mut self, field: &Field, value: Value) {
    if field.name() == "message" {
      self.event = Some(match value {
        Value::String(text) => text,
        other => other.to_string(),
      });
    } else {
      self.fields.insert(field.name().to_string(), value);
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

  use super::targets_filter;

  struct Capture(Arc<Mutex<Vec<(String, String)>>>);

  impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
      self.0.lock().unwrap().push((event.metadata().target().to_string(), event.metadata().level().to_string()));
    }
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
}
