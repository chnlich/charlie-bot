//! Writes a synthetic session home: the sessions, their event logs, and the manifest that describes them.
//!
//! The default shape writes each session's event log at its planned size (`crate::session`). The task-tree shape
//! (`crate::task_tree`) writes sessions of the same sizes as the task nodes of a production startup.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use charliebot_core::event_types;
use charliebot_store::{layout, metadata, run_records};
use chrono::{NaiveDateTime, TimeDelta};
use serde_json::{Value, json};

use crate::Shape;
use crate::manifest::{MANIFEST_NAME, Manifest, ManifestSession, SessionMetadata};
use crate::rng::Rng;
use crate::session::{
  EventLog, SESSION_START_SPREAD_SECONDS, event_stamp, final_event, first_session_time, planned_home, session_id,
  stamp, write_json,
};
use crate::task_tree;
use crate::text::{self, SyntheticText};

/// Writes a home of `shape` with `sessions` sessions for `seed` into `output`, which must be absent or empty.
///
/// The home is built in a staging directory beside `output` and renamed into place when it is complete, so no reader
/// sees a partial home. Two runs that finish together yield the same bytes; the one that comes second keeps the first's
/// home when the manifest matches its seed, session count and shape.
pub fn generate(output: &Path, sessions: usize, seed: u64, shape: Shape) -> Result<()> {
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

  let written = match shape {
    Shape::Default | Shape::View => write_home(&staging, sessions, seed, shape),
    Shape::TaskTree => {
      let home = fs::canonicalize(parent)
        .with_context(|| format!("cannot resolve {}", parent.display()))
        .map(|parent| parent.join(name.as_ref()));
      home.and_then(|home| task_tree::write_home(&staging, &home, sessions, seed))
    }
  };
  let total = match written {
    Ok(total) => total,
    Err(error) => {
      remove_staging(&staging);
      return Err(error);
    }
  };
  if let Err(error) = fs::rename(&staging, output) {
    remove_staging(&staging);
    if finished_by_another_run(output, sessions, seed, shape) {
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

/// Whether `output` holds a finished home for this seed, session count and shape. A home is renamed into place only when
/// it is complete, so its manifest is the last file a run leaves there.
fn finished_by_another_run(output: &Path, sessions: usize, seed: u64, shape: Shape) -> bool {
  Manifest::read(output)
    .is_ok_and(|manifest| manifest.seed == seed && manifest.session_count == sessions && manifest.shape == shape)
}

/// The count of acceptance criteria and of context references of a view session, each drawn from these bounds.
const VIEW_ITEMS_MIN: u64 = 8;
const VIEW_ITEMS_MAX: u64 = 28;

/// One user message whose line takes exactly `room` bytes, or `None` when `room` cannot hold an empty one.
fn filler_event(room: u64, timestamp: &str) -> Option<Value> {
  let empty =
    run_records::event_line(&json!({"type": event_types::USER, "content": "", "timestamp": timestamp})).len() as u64;
  if room < empty {
    return None;
  }
  // The source is ASCII, so a byte count is a character count and the cut leaves valid text.
  let source = text::filler_source();
  let repeated = source.repeat(room as usize / source.len() + 1);
  let content = &repeated[..(room - empty) as usize];
  Some(json!({"type": event_types::USER, "content": content, "timestamp": timestamp}))
}

/// Writes events into one log of exactly `target` bytes, ending with one final user message.
///
/// The last gap is one filler message, so the log takes the planned size and not a size that the last event
/// overshoots. Returns the bytes and the event count.
fn write_event_log(
  path: &Path,
  target: u64,
  rng: &mut Rng,
  text: &SyntheticText,
  started_at: NaiveDateTime,
  final_text: &str,
) -> Result<(u64, u64)> {
  let target = target as i64;
  let final_bytes = run_records::event_line(&final_event(final_text, &event_stamp(started_at, 0))).len() as i64;
  let mut log = EventLog::create(path, started_at)?;
  log.write_turns(target - final_bytes, rng, text, &mut |_, _| {})?;
  let room = target - log.written() - final_bytes;
  if room > 0
    && let Some(filler) = filler_event(room as u64, &log.next_stamp())
  {
    log.push(&filler)?;
  }
  log.push(&final_event(final_text, &log.next_stamp()))?;
  log.finish()
}

/// The acceptance criteria and context references of a view session: counts drawn from the stream, each item a sentence
/// or a path of the synthetic pool. The spread of the counts gives the rows a spread of sizes; their median sets the
/// view's median row.
fn view_task(rng: &mut Rng) -> Value {
  let acceptance: Vec<&str> = (0..rng.range(VIEW_ITEMS_MIN, VIEW_ITEMS_MAX)).map(|_| text::phrase(rng)).collect();
  let context_refs: Vec<String> = (0..rng.range(VIEW_ITEMS_MIN, VIEW_ITEMS_MAX))
    .map(|_| format!("workspace/synthetic/{:04x}/{:08x}.md", rng.below(65_536), rng.next_u64() as u32))
    .collect();
  json!({"acceptance": acceptance, "context_refs": context_refs, (metadata::REPO_PATH_FIELD): "workspace/synthetic",
         (metadata::BASE_BRANCH_FIELD): "main"})
}

/// Writes the sessions and the manifest into `home`, and returns the total event-log bytes. The view shape adds a task
/// spec to each session; the other shapes write the same bytes.
fn write_home(home: &Path, sessions: usize, seed: u64, shape: Shape) -> Result<u64> {
  let view = shape == Shape::View;
  let (mut rng, text, sizes) = planned_home(sessions, seed);
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
      task: view.then(|| view_task(&mut rng)),
    };
    write_json(&layout::metadata_path(&session_dir), &metadata, b"  ")?;
    manifest_sessions.push(ManifestSession { id, event_log_bytes: log_bytes, last_message: final_text });
    total += log_bytes;
  }
  let manifest = Manifest { seed, session_count: sessions, event_log_bytes: total, sessions: manifest_sessions, shape };
  write_json(&home.join(MANIFEST_NAME), &manifest, b" ")?;
  Ok(total)
}
