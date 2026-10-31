//! Writes a synthetic session home: the sessions, their event logs, and the manifest that describes them.
//!
//! The event-log size of each session follows the production shape. `SIZE_KNOTS` gives the size at each quantile,
//! log-linear between knots, and `planned_sizes` deals those sizes to the sessions in a seeded order.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use charliebot_store::layout;
use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use serde::Serialize;
use serde_json::{Value, json};

use crate::manifest::{MANIFEST_NAME, Manifest, ManifestSession, SessionMetadata};
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
const SESSION_START_SPREAD_SECONDS: u64 = 200 * 86_400;
/// Above this many bytes left to fill, an assistant event carries long answers.
const LONG_EVENT_BUDGET: u64 = 200_000;
/// A log's last gap smaller than this takes no more events; one filler message fills the gap.
const FILLER_THRESHOLD_BYTES: i64 = 4096;

/// Writes a home of `sessions` sessions for `seed` into `output`, which must be absent or empty.
///
/// The home is built in a staging directory beside `output` and renamed into place when it is complete, so no reader
/// sees a partial home. Two runs that finish together yield the same bytes; the one that comes second keeps the first's
/// home when the manifest matches its seed and session count.
pub fn generate(output: &Path, sessions: usize, seed: u64) -> Result<()> {
  if output.exists() && fs::read_dir(output)?.next().is_some() {
    bail!("output directory is not empty: {}", output.display());
  }
  let parent = match output.parent() {
    Some(parent) if !parent.as_os_str().is_empty() => parent,
    _ => Path::new("."),
  };
  fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
  let name = output.file_name().context("the output path names no directory")?.to_string_lossy();
  let staging = parent.join(format!(".{name}.staging-{}", std::process::id()));
  fs::create_dir(&staging).with_context(|| format!("cannot create the staging directory {}", staging.display()))?;

  let total = match write_home(&staging, sessions, seed) {
    Ok(total) => total,
    Err(error) => {
      remove_staging(&staging);
      return Err(error);
    }
  };
  if let Err(error) = fs::rename(&staging, output) {
    remove_staging(&staging);
    if finished_by_another_run(output, sessions, seed) {
      println!("{} already holds this home: seed {seed}, {sessions} sessions", output.display());
      return Ok(());
    }
    return Err(error).with_context(|| format!("cannot move the home into {}", output.display()));
  }
  let gib = total as f64 / (1u64 << 30) as f64;
  println!("generated {sessions} sessions, {total} event-log bytes ({gib:.2} GiB) in {}", output.display());
  Ok(())
}

fn remove_staging(staging: &Path) {
  if let Err(error) = fs::remove_dir_all(staging) {
    eprintln!("mock-home: cannot remove the staging directory {}: {error}", staging.display());
  }
}

/// Whether `output` holds a finished home for this seed and session count. A home is renamed into place only when it is
/// complete, so its manifest is the last file a run leaves there.
fn finished_by_another_run(output: &Path, sessions: usize, seed: u64) -> bool {
  Manifest::read(output).is_ok_and(|manifest| manifest.seed == seed && manifest.session_count == sessions)
}

fn first_session_time() -> NaiveDateTime {
  NaiveDate::from_ymd_opt(2025, 9, 1).and_then(|date| date.and_hms_opt(0, 0, 0)).expect("the anchor is a valid date")
}

/// The timestamp of a session or metadata record, with microseconds.
fn stamp(moment: NaiveDateTime) -> String {
  moment.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

/// The timestamp of event `index` of a session that started at `started_at`: one event per second.
fn event_stamp(started_at: NaiveDateTime, index: u64) -> String {
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

/// One event as its JSON line, with the newline.
fn encode_event(event: &Value) -> Vec<u8> {
  let mut line = serde_json::to_vec(event).expect("a JSON value serializes");
  line.push(b'\n');
  line
}

fn final_event(final_text: &str, timestamp: &str) -> Value {
  json!({"type": "user", "content": final_text, "timestamp": timestamp})
}

/// One random event. Above `LONG_EVENT_BUDGET` bytes left, its content is longer.
fn next_event(rng: &mut Rng, text: &SyntheticText, timestamp: &str, budget: u64) -> Value {
  let kind = rng.unit();
  if kind < 0.12 {
    let content = text::paragraph(rng);
    return json!({"type": "user", "content": content, "timestamp": timestamp});
  }
  if kind < 0.52 {
    let count = if budget > LONG_EVENT_BUDGET { rng.range(4, 40) } else { rng.range(1, 4) };
    let blocks: Vec<&str> = (0..count).map(|_| text.block(rng)).collect();
    let body = blocks.join("\n\n");
    return json!({"type": "assistant", "message": {"content": [{"type": "text", "text": body}]}, "timestamp": timestamp});
  }
  if kind < 0.8 {
    let (tool_name, tool_input) = text::tool_call(rng);
    let id = text::tool_id(rng);
    let tool_use = json!({"type": "tool_use", "id": id, "name": tool_name, "input": tool_input});
    let content = if rng.unit() < 0.5 {
      vec![json!({"type": "text", "text": text::phrase(rng)}), tool_use]
    } else {
      vec![tool_use]
    };
    return json!({"type": "assistant", "message": {"content": content}, "timestamp": timestamp});
  }
  let lines = if budget > LONG_EVENT_BUDGET { rng.range(1, 40) } else { rng.range(1, 4) };
  let tool_use_id = text::tool_id(rng);
  let content = text.tool_output(rng, lines);
  let result = json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": false});
  json!({"type": "user", "message": {"content": [result]}, "timestamp": timestamp})
}

/// Whether `event` starts a turn: a plain user message, not a tool result.
fn opens_turn(event: &Value) -> bool {
  event.get("type").and_then(Value::as_str) == Some("user") && event.get("content").is_some()
}

/// One turn separator in the production `master_done` shape that the renderer turns into a separator message.
fn master_done_event(rng: &mut Rng, timestamp: &str) -> Value {
  json!({
    "type": "master_done",
    "exit_code": 0,
    "still_thinking": false,
    "thinking_seconds": rng.range(1, 120),
    "timestamp": timestamp,
  })
}

/// One user message whose line takes exactly `room` bytes, or `None` when `room` cannot hold an empty one.
fn filler_event(room: u64, timestamp: &str) -> Option<Value> {
  let empty = encode_event(&json!({"type": "user", "content": "", "timestamp": timestamp})).len() as u64;
  if room < empty {
    return None;
  }
  // The source is ASCII, so a byte count is a character count and the cut leaves valid text.
  let source = text::filler_source();
  let repeated = source.repeat(room as usize / source.len() + 1);
  let content = &repeated[..(room - empty) as usize];
  Some(json!({"type": "user", "content": content, "timestamp": timestamp}))
}

/// Writes events into one log of exactly `target` bytes, ending with one final user message.
///
/// Each turn ends with one `master_done` separator, and a plain user message closes the turn before it. `tail(40)`
/// snaps to a turn start through these separators, so a seeded log renders a bounded page like a production log
/// instead of its whole history. The last gap is one filler message, so the log takes the planned size and not a
/// size that the last event overshoots. Returns the bytes and the event count.
fn write_event_log(
  path: &Path,
  target: u64,
  rng: &mut Rng,
  text: &SyntheticText,
  started_at: NaiveDateTime,
  final_text: &str,
) -> Result<(u64, u64)> {
  let target = target as i64;
  let final_bytes = encode_event(&final_event(final_text, &event_stamp(started_at, 0))).len() as i64;
  let mut log = BufWriter::new(File::create(path).with_context(|| format!("cannot create {}", path.display()))?);
  let mut written: i64 = 0;
  let mut count: u64 = 0;
  let mut turn_open = false;
  loop {
    let room = target - written - final_bytes;
    if room < FILLER_THRESHOLD_BYTES {
      break;
    }
    let event = next_event(rng, text, &event_stamp(started_at, count), room as u64);
    let stimulus = opens_turn(&event);
    if stimulus && turn_open {
      let separator = encode_event(&master_done_event(rng, &event_stamp(started_at, count)));
      if separator.len() as i64 > room {
        break;
      }
      log.write_all(&separator)?;
      written += separator.len() as i64;
      count += 1;
      turn_open = false;
    }
    let room = target - written - final_bytes;
    let line = encode_event(&event);
    if line.len() as i64 > room {
      break;
    }
    log.write_all(&line)?;
    written += line.len() as i64;
    count += 1;
    if !stimulus {
      turn_open = true;
    }
  }
  if turn_open {
    let separator = encode_event(&master_done_event(rng, &event_stamp(started_at, count)));
    if separator.len() as i64 <= target - written - final_bytes {
      log.write_all(&separator)?;
      written += separator.len() as i64;
      count += 1;
    }
  }
  let room = target - written - final_bytes;
  if room > 0
    && let Some(filler) = filler_event(room as u64, &event_stamp(started_at, count))
  {
    let line = encode_event(&filler);
    log.write_all(&line)?;
    written += line.len() as i64;
    count += 1;
  }
  let line = encode_event(&final_event(final_text, &event_stamp(started_at, count)));
  log.write_all(&line)?;
  written += line.len() as i64;
  count += 1;
  log.flush()?;
  Ok((written as u64, count))
}

/// A random version-4 session id, drawn from the seeded stream.
fn session_id(rng: &mut Rng) -> String {
  let mut bytes = [0u8; 16];
  bytes[..8].copy_from_slice(&rng.next_u64().to_be_bytes());
  bytes[8..].copy_from_slice(&rng.next_u64().to_be_bytes());
  uuid::Builder::from_random_bytes(bytes).into_uuid().to_string()
}

/// Writes one JSON file with the given indent, and no newline at the end.
fn write_json<T: Serialize>(path: &Path, value: &T, indent: &[u8]) -> Result<()> {
  let mut bytes = Vec::new();
  let formatter = serde_json::ser::PrettyFormatter::with_indent(indent);
  let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
  value.serialize(&mut serializer).with_context(|| format!("cannot encode {}", path.display()))?;
  fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))
}

/// Writes the sessions and the manifest into `home`, and returns the total event-log bytes.
fn write_home(home: &Path, sessions: usize, seed: u64) -> Result<u64> {
  let mut rng = Rng::new(seed);
  let text = SyntheticText::new(&mut rng);
  let sizes = planned_sizes(sessions, &mut rng);
  let mut manifest_sessions = Vec::with_capacity(sessions);
  let mut total = 0;
  for size in sizes {
    let id = session_id(&mut rng);
    let session_dir = home.join(&id);
    fs::create_dir_all(layout::data_dir(&session_dir))
      .with_context(|| format!("cannot create {}", session_dir.display()))?;
    let offset = rng.range(0, SESSION_START_SPREAD_SECONDS);
    let started_at = first_session_time() + TimeDelta::seconds(offset as i64);
    let final_text = format!("Checkpoint {id}: {}", text::phrase(&mut rng));
    let log_path = layout::chat_events_path(&session_dir);
    let (log_bytes, event_count) = write_event_log(&log_path, size, &mut rng, &text, started_at, &final_text)?;
    let metadata = SessionMetadata {
      id: id.clone(),
      name: text::title(&mut rng),
      status: "active",
      profile: "manager",
      backend: "mock-backend",
      created_at: stamp(started_at),
      updated_at: stamp(started_at + TimeDelta::seconds(event_count as i64)),
      schema_version: 2,
      has_unread: false,
      starred: false,
    };
    write_json(&layout::metadata_path(&session_dir), &metadata, b"  ")?;
    manifest_sessions.push(ManifestSession { id, event_log_bytes: log_bytes, last_message: final_text });
    total += log_bytes;
  }
  let manifest = Manifest { seed, session_count: sessions, event_log_bytes: total, sessions: manifest_sessions };
  write_json(&home.join(MANIFEST_NAME), &manifest, b" ")?;
  Ok(total)
}
