//! The parts of one synthetic session that both home shapes write: its planned event-log size, its id and times, the
//! event-log writer and the JSON writer.
//!
//! The event-log size of each session follows the production shape. `SIZE_KNOTS` gives the size at each quantile,
//! log-linear between knots, and `planned_sizes` deals those sizes to the sessions in a seeded order.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use charliebot_core::event_types;
use charliebot_store::run_records;
use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use serde::Serialize;
use serde_json::{Value, json};

use crate::Language;
use crate::rng::Rng;
use crate::text::{self, SyntheticText};

/// The event-log size at each quantile, as (quantile, bytes). Each knot is a production measurement: 1.7 KB at the
/// median, 1.4 MB at p90, 17 MB at p99 and 144 MB at the maximum. The maximum is the largest of 7,160 sessions.
const SIZE_KNOTS: [(f64, f64); 5] = [(0.0, 150.0), (0.5, 1.7e3), (0.9, 1.4e6), (0.99, 1.7e7), (1.0, 1.44e8)];
/// The exponent of each segment's position curve. An upper segment's exponent sets how its sizes bunch toward the lower
/// knot. These exponents give a total of about 5.8 GB at 7,160 sessions.
const SEGMENT_EXPONENTS: [f64; 4] = [1.0, 1.0, 2.0, 4.0];
/// Each session starts within this many seconds after the anchor. The fixed anchor keeps the seed reproducible and
/// keeps the pages from showing sessions dated after the run.
pub(crate) const SESSION_START_SPREAD_SECONDS: u64 = 200 * 86_400;
/// Above this many bytes left to fill, an assistant event carries long answers.
const LONG_EVENT_BUDGET: u64 = 200_000;
/// A log's last gap smaller than this takes no more events; one filler message fills the gap.
const FILLER_THRESHOLD_BYTES: i64 = 4096;

pub(crate) fn first_session_time() -> NaiveDateTime {
  NaiveDate::from_ymd_opt(2025, 9, 1).and_then(|date| date.and_hms_opt(0, 0, 0)).expect("the anchor is a valid date")
}

/// The timestamp of a session or metadata record, with microseconds.
pub(crate) fn stamp(moment: NaiveDateTime) -> String {
  moment.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

/// The timestamp of event `index` of a session that started at `started_at`: one event per second.
pub(crate) fn event_stamp(started_at: NaiveDateTime, index: u64) -> String {
  (started_at + TimeDelta::seconds(index as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The event-log size in bytes at quantile `u`, for 0 <= u <= 1.
fn event_log_size(u: f64) -> f64 {
  for (segment, exponent) in SEGMENT_EXPONENTS.iter().enumerate() {
    let (u_low, size_low) = SIZE_KNOTS[segment];
    let (u_high, size_high) = SIZE_KNOTS[segment + 1];
    if u <= u_high {
      let position = (u - u_low) / (u_high - u_low);
      let log_size = size_low.ln() + (size_high.ln() - size_low.ln()) * position.powf(*exponent);
      return log_size.exp();
    }
  }
  unreachable!("the quantile {u} lies in [0, 1], which the last knot covers")
}

/// `count` sizes at evenly spaced quantiles, in a seeded order, so the seed decides which session gets which size.
fn planned_sizes(count: usize, rng: &mut Rng) -> Vec<u64> {
  let mut sizes: Vec<u64> = (0..count)
    .map(|index| {
      let u = if count > 1 { index as f64 / (count - 1) as f64 } else { 0.5 };
      event_log_size(u).round_ties_even() as u64
    })
    .collect();
  rng.shuffle(&mut sizes);
  sizes
}

/// The seeded stream of a home of `sessions` sessions, after it draws the text pools and the event-log sizes.
/// The tuple holds the stream, the pools and the sizes in session order.
pub(crate) fn planned_home(sessions: usize, seed: u64, language: Language) -> (Rng, SyntheticText, Vec<u64>) {
  let mut rng = Rng::new(seed);
  let text = SyntheticText::new(&mut rng, language);
  let sizes = planned_sizes(sessions, &mut rng);
  (rng, text, sizes)
}

pub(crate) fn final_event(final_text: &str, timestamp: &str) -> Value {
  json!({"type": event_types::USER, "content": final_text, "timestamp": timestamp})
}

/// One random event. Above `LONG_EVENT_BUDGET` bytes left, its content is longer.
fn next_event(rng: &mut Rng, text: &SyntheticText, timestamp: &str, budget: u64) -> Value {
  let kind = rng.unit();
  if kind < 0.12 {
    let content = text::paragraph(rng, text.language());
    return json!({"type": event_types::USER, "content": content, "timestamp": timestamp});
  }
  if kind < 0.52 {
    let count = if budget > LONG_EVENT_BUDGET { rng.range(4, 40) } else { rng.range(1, 4) };
    let blocks: Vec<&str> = (0..count).map(|_| text.block(rng)).collect();
    let body = blocks.join("\n\n");
    return json!({
      "type": event_types::ASSISTANT,
      "message": {"content": [{"type": "text", "text": body}]},
      "timestamp": timestamp
    });
  }
  if kind < 0.8 {
    let (tool_name, tool_input) = text::tool_call(rng);
    let id = text::tool_id(rng);
    let tool_use = json!({"type": event_types::TOOL_USE, "id": id, "name": tool_name, "input": tool_input});
    let content = if rng.unit() < 0.5 {
      vec![json!({"type": "text", "text": text::phrase(rng, text.language())}), tool_use]
    } else {
      vec![tool_use]
    };
    return json!({"type": event_types::ASSISTANT, "message": {"content": content}, "timestamp": timestamp});
  }
  let lines = if budget > LONG_EVENT_BUDGET { rng.range(1, 40) } else { rng.range(1, 4) };
  let tool_use_id = text::tool_id(rng);
  let content = text.tool_output(rng, lines);
  let result = json!({
    "type": event_types::TOOL_RESULT,
    event_types::TOOL_USE_ID: tool_use_id,
    "content": content,
    "is_error": false
  });
  json!({"type": event_types::USER, "message": {"content": [result]}, "timestamp": timestamp})
}

/// Whether `event` starts a turn: a plain user message, not a tool result.
fn opens_turn(event: &Value) -> bool {
  event.get("type").and_then(Value::as_str) == Some(event_types::USER) && event.get("content").is_some()
}

/// One turn separator in the production `master_done` shape that the renderer turns into a separator message.
fn master_done_event(rng: &mut Rng, timestamp: &str) -> Value {
  json!({
    "type": event_types::MASTER_DONE,
    "exit_code": 0,
    event_types::STILL_THINKING: false,
    event_types::THINKING_SECONDS: rng.range(1, 120),
    "timestamp": timestamp,
  })
}

/// One event log being written: its file, the clock of its events, and the bytes and events written so far.
pub(crate) struct EventLog {
  out: BufWriter<File>,
  started_at: NaiveDateTime,
  written: i64,
  count: u64,
}

impl EventLog {
  /// Creates the log at `path` for a session that started at `started_at`.
  pub(crate) fn create(path: &Path, started_at: NaiveDateTime) -> Result<Self> {
    let out = BufWriter::new(File::create(path).with_context(|| format!("cannot create {}", path.display()))?);
    Ok(Self { out, started_at, written: 0, count: 0 })
  }

  /// The bytes written so far.
  pub(crate) fn written(&self) -> i64 {
    self.written
  }

  /// The timestamp of the next event: one event per second from the session start.
  pub(crate) fn next_stamp(&self) -> String {
    event_stamp(self.started_at, self.count)
  }

  /// Writes one event as one line.
  pub(crate) fn push(&mut self, event: &Value) -> Result<()> {
    let line = run_records::event_line(event);
    self.out.write_all(&line)?;
    self.written += line.len() as i64;
    self.count += 1;
    Ok(())
  }

  /// Writes turns of random events while at least `FILLER_THRESHOLD_BYTES` remain below `limit` bytes.
  ///
  /// A plain user message opens a turn, and one `master_done` separator closes the turn before it and the last turn.
  /// `tail(40)` snaps to a turn start through these separators, so a seeded log renders a bounded page like a
  /// production log instead of its whole history. `label` sees each turn-opening message before it is written.
  pub(crate) fn write_turns(
    &mut self,
    limit: i64,
    rng: &mut Rng,
    text: &SyntheticText,
    label: &mut dyn FnMut(&mut Rng, &mut Value),
  ) -> Result<()> {
    let mut turn_open = false;
    loop {
      let room = limit - self.written;
      if room < FILLER_THRESHOLD_BYTES {
        break;
      }
      let timestamp = self.next_stamp();
      let mut event = next_event(rng, text, &timestamp, room as u64);
      let stimulus = opens_turn(&event);
      if stimulus && turn_open {
        let separator = master_done_event(rng, &timestamp);
        if run_records::event_line(&separator).len() as i64 > room {
          break;
        }
        self.push(&separator)?;
        turn_open = false;
      }
      if stimulus {
        label(rng, &mut event);
      }
      if run_records::event_line(&event).len() as i64 > limit - self.written {
        break;
      }
      self.push(&event)?;
      if !stimulus {
        turn_open = true;
      }
    }
    if turn_open {
      let separator = master_done_event(rng, &self.next_stamp());
      if run_records::event_line(&separator).len() as i64 <= limit - self.written {
        self.push(&separator)?;
      }
    }
    Ok(())
  }

  /// Flushes the log and returns its bytes and event count.
  pub(crate) fn finish(mut self) -> Result<(u64, u64)> {
    self.out.flush()?;
    Ok((self.written as u64, self.count))
  }
}

/// A random version-4 session id, drawn from the seeded stream.
pub(crate) fn session_id(rng: &mut Rng) -> String {
  let mut bytes = [0u8; 16];
  bytes[..8].copy_from_slice(&rng.next_u64().to_be_bytes());
  bytes[8..].copy_from_slice(&rng.next_u64().to_be_bytes());
  uuid::Builder::from_random_bytes(bytes).into_uuid().to_string()
}

/// Writes one JSON file with the given indent, and no newline at the end.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T, indent: &[u8]) -> Result<()> {
  let mut bytes = Vec::new();
  let formatter = serde_json::ser::PrettyFormatter::with_indent(indent);
  let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
  value.serialize(&mut serializer).with_context(|| format!("cannot encode {}", path.display()))?;
  fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))
}
