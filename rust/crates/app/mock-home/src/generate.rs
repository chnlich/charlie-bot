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

use crate::Language;
use crate::Shape;
use crate::cron::{self, BOUND_SESSIONS};
use crate::manifest::{MANIFEST_NAME, Manifest, ManifestSession, SessionMetadata};
use crate::rng::Rng;
use crate::session::{
  EventLog, SESSION_START_SPREAD_SECONDS, event_stamp, final_event, first_session_time, planned_home, session_id,
  stamp, write_json,
};
use crate::task_tree;
use crate::text::{self, SyntheticText};

/// Writes a home of `shape` with `sessions` sessions for `seed` into `output`, which must be absent or empty. Every
/// seeded sentence, title and label reads in `language`.
///
/// The home is built in a staging directory beside `output` and renamed into place when it is complete, so no reader
/// sees a partial home. Two runs that finish together yield the same bytes; the one that comes second keeps the first's
/// home when the manifest matches its seed, session count, shape and language.
pub fn generate(output: &Path, sessions: usize, seed: u64, shape: Shape, language: Language) -> Result<()> {
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
    Shape::Default | Shape::View | Shape::Search => write_home(&staging, sessions, seed, shape, language),
    Shape::TaskTree | Shape::ServeTaskTree => {
      let home = fs::canonicalize(parent)
        .with_context(|| format!("cannot resolve {}", parent.display()))
        .map(|parent| parent.join(name.as_ref()));
      home.and_then(|home| task_tree::write_home(&staging, &home, sessions, seed, language, shape))
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
    if finished_by_another_run(output, sessions, seed, shape, language) {
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

/// Whether `output` holds a finished home for this seed, session count, shape and language. A home is renamed into place
/// only when it is complete, so its manifest is the last file a run leaves there.
fn finished_by_another_run(output: &Path, sessions: usize, seed: u64, shape: Shape, language: Language) -> bool {
  Manifest::read(output).is_ok_and(|manifest| {
    manifest.seed == seed
      && manifest.session_count == sessions
      && manifest.shape == shape
      && manifest.language == language
  })
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
fn view_task(rng: &mut Rng, language: Language) -> Value {
  let acceptance: Vec<&str> =
    (0..rng.range(VIEW_ITEMS_MIN, VIEW_ITEMS_MAX)).map(|_| text::phrase(rng, language)).collect();
  let context_refs: Vec<String> = (0..rng.range(VIEW_ITEMS_MIN, VIEW_ITEMS_MAX))
    .map(|_| format!("workspace/synthetic/{:04x}/{:08x}.md", rng.below(65_536), rng.next_u64() as u32))
    .collect();
  json!({(metadata::ACCEPTANCE_FIELD): acceptance, (metadata::CONTEXT_REFS_FIELD): context_refs,
         (metadata::REPO_PATH_FIELD): "workspace/synthetic",
         (metadata::BASE_BRANCH_FIELD): "main"})
}

/// One query's match set in the `search` shape: `sessions` archived sessions whose names carry `query`, each with a
/// `web_status` goal of `goal_chars` Han characters.
struct MatchSet {
  query: &'static str,
  sessions: u64,
  goal_chars: usize,
}

/// The match sets of the `search` shape. The server answers a search with at most 200 rows (`SEARCH_RESULT_LIMIT` in
/// `runtime/session-api/src/search.rs`), so a set of 200 sessions fills an answer. A row carries its session's
/// `web_status` goal, and the goal's length sets the answer's gzip body: each count lands the body within 10 % of the
/// live before capture of the same query (dev/poor-network/README.md, "Search view").
const SEARCH_MATCH_SETS: [MatchSet; 4] = [
  MatchSet { query: "sidebar", sessions: 200, goal_chars: 229 },
  MatchSet { query: "plan", sessions: 200, goal_chars: 119 },
  MatchSet { query: "cron", sessions: 200, goal_chars: 105 },
  MatchSet { query: "hydeogen", sessions: 2, goal_chars: 250 },
];
/// The Han block the goal characters are drawn from: U+4E00 to U+9FFF, every code point assigned.
const HAN_FIRST: u64 = 0x4E00;
const HAN_COUNT: u64 = 0x9FFF - 0x4E00 + 1;
/// Days after the view's sessions start at which the match sets are stamped. The answer keeps its newest 200 name hits,
/// so each set's sessions must be newer than every view session.
const SEARCH_MATCH_STAMP_DAYS: i64 = 30;

/// `count` Han characters drawn from the seeded stream. Each takes three bytes, and gzip keeps about 0.6 of them, so the
/// count sets a row's share of the answer's compressed body.
fn han_text(rng: &mut Rng, count: usize) -> String {
  (0..count)
    .map(|_| char::from_u32((HAN_FIRST + rng.below(HAN_COUNT)) as u32).expect("the Han block holds scalar values"))
    .collect()
}

/// The `web_status` of a session whose goal is `goal`. The search match sets and the default shape's newest rows
/// write it.
fn web_status_of(goal: String) -> Value {
  json!({"goal": goal, "now": "", "waiting": []})
}

/// The rows of a default home that carry a `web_status` goal. The live store had 5 of its newest 200 listing rows
/// with a goal at implementation, and the goal's length is the median of those 5 goals, in characters.
const NEWEST_WEB_STATUS_ROWS: usize = 5;
const NEWEST_WEB_STATUS_GOAL_CHARS: usize = 143;

/// Gives a goal to the newest `NEWEST_WEB_STATUS_ROWS` of `rows`. The listing orders its rows by `updated_at`, newest
/// first, and then by `id`, newest first, so the newest rows are those that come first in that order.
fn give_newest_web_status(rows: &mut [SessionMetadata], rng: &mut Rng) {
  rows.sort_by(|left, right| (&right.updated_at, &right.id).cmp(&(&left.updated_at, &left.id)));
  for row in rows.iter_mut().take(NEWEST_WEB_STATUS_ROWS) {
    row.web_status = Some(web_status_of(han_text(rng, NEWEST_WEB_STATUS_GOAL_CHARS)));
  }
}

/// Writes the `search` shape's match sets into `home`: archived sessions named for each query, each with a task spec, a
/// `web_status` goal of the set's size, and an empty chat file. The active list the bench counts leaves archived sessions
/// out, and a search answer reads a name hit's chat file only when it has no name hits to fill the answer.
fn write_search_matches(home: &Path, rng: &mut Rng, language: Language) -> Result<()> {
  let stamped_at = first_session_time()
    + TimeDelta::seconds(SESSION_START_SPREAD_SECONDS as i64)
    + TimeDelta::days(SEARCH_MATCH_STAMP_DAYS);
  for set in SEARCH_MATCH_SETS {
    for index in 0..set.sessions {
      let id = session_id(rng);
      let session_dir = home.join(&id);
      fs::create_dir_all(layout::data_dir(&session_dir))
        .with_context(|| format!("cannot create {}", session_dir.display()))?;
      let stamp_text = stamp(stamped_at + TimeDelta::seconds(index as i64));
      let goal = han_text(rng, set.goal_chars);
      let metadata = SessionMetadata {
        id,
        name: format!("{} match {index:03}", set.query),
        status: "archived",
        profile: metadata::TaskProfile::Manager.as_str(),
        backend: "mock-backend",
        created_at: stamp_text.clone(),
        updated_at: stamp_text,
        schema_version: 2,
        has_unread: false,
        starred: false,
        task: Some(view_task(rng, language)),
        web_status: Some(web_status_of(goal)),
      };
      write_json(&layout::metadata_path(&session_dir), &metadata, b"  ")?;
      let chat = layout::chat_events_path(&session_dir);
      fs::write(&chat, b"").with_context(|| format!("cannot write {}", chat.display()))?;
    }
  }
  Ok(())
}

/// The manager sessions that the default shape's cron fixture binds: the middle rows of the listing order. The listing
/// is sorted newest first by `give_newest_web_status`, so these rows stay out of the newest rows that the perf
/// measurement reads. A home smaller than the fixture repeats its rows, so tasks share a session.
fn cron_bound_sessions(listed: &[SessionMetadata]) -> Result<[String; BOUND_SESSIONS]> {
  if listed.is_empty() {
    bail!("the default home holds no session for its cron fixture to bind");
  }
  let start = listed.len().saturating_sub(BOUND_SESSIONS) / 2;
  Ok(std::array::from_fn(|index| listed[(start + index) % listed.len()].id.clone()))
}

/// Writes the sessions and the manifest into `home`, and returns the total event-log bytes. The view and search shapes
/// add a task spec to each session; the other shapes write the same bytes. The default shape gives the newest rows a
/// `web_status` goal and binds its cron fixture to four manager sessions; the view and search shapes keep their bytes,
/// because the bench reads them. The search shape adds its match sets after the view's sessions, which the manifest does
/// not list: the bench counts the active sessions, and these are archived.
fn write_home(home: &Path, sessions: usize, seed: u64, shape: Shape, language: Language) -> Result<u64> {
  let view = matches!(shape, Shape::View | Shape::Search);
  let (mut rng, text, sizes) = planned_home(sessions, seed, language);
  let mut manifest_sessions = Vec::with_capacity(sessions);
  let mut listed = Vec::with_capacity(sessions);
  let mut total = 0;
  for size in sizes {
    let id = session_id(&mut rng);
    let session_dir = home.join(&id);
    fs::create_dir_all(layout::data_dir(&session_dir))
      .with_context(|| format!("cannot create {}", session_dir.display()))?;
    let offset = rng.range(0, SESSION_START_SPREAD_SECONDS);
    let started_at = first_session_time() + TimeDelta::seconds(offset as i64);
    let final_text = format!("Checkpoint {id}: {}", text::phrase(&mut rng, language));
    let log_path = layout::chat_events_path(&session_dir);
    let (log_bytes, event_count) = write_event_log(&log_path, size, &mut rng, &text, started_at, &final_text)?;
    let metadata = SessionMetadata {
      id: id.clone(),
      name: text::title(&mut rng, language),
      status: "active",
      profile: metadata::TaskProfile::Manager.as_str(),
      backend: "mock-backend",
      created_at: stamp(started_at),
      updated_at: stamp(started_at + TimeDelta::seconds(event_count as i64)),
      schema_version: 2,
      has_unread: false,
      starred: false,
      task: view.then(|| view_task(&mut rng, language)),
      web_status: None,
    };
    listed.push(metadata);
    manifest_sessions.push(ManifestSession { id, event_log_bytes: log_bytes, last_message: Some(final_text) });
    total += log_bytes;
  }
  if shape == Shape::Default {
    give_newest_web_status(&mut listed, &mut rng);
  }
  for metadata in &listed {
    write_json(&layout::metadata_path(&home.join(&metadata.id)), metadata, b"  ")?;
  }
  let cron_tasks =
    if shape == Shape::Default { cron::write_fixture(home, &cron_bound_sessions(&listed)?)? } else { Vec::new() };
  if shape == Shape::Search {
    write_search_matches(home, &mut rng, language)?;
  }
  let manifest = Manifest {
    seed,
    session_count: sessions,
    event_log_bytes: total,
    sessions: manifest_sessions,
    shape,
    language,
    cron_tasks,
  };
  write_json(&home.join(MANIFEST_NAME), &manifest, b" ")?;
  Ok(total)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A stamp in the form `stamp` writes, `second` seconds into one minute.
  fn at(second: u32) -> String {
    format!("2026-11-19T00:00:{second:02}.000000Z")
  }

  /// A listing row that differs from the others only in its id and its `updated_at`.
  fn row(id: &str, second: u32) -> SessionMetadata {
    SessionMetadata {
      id: id.to_string(),
      name: String::new(),
      status: "active",
      profile: metadata::TaskProfile::Manager.as_str(),
      backend: "mock-backend",
      created_at: at(second),
      updated_at: at(second),
      schema_version: 2,
      has_unread: false,
      starred: false,
      task: None,
      web_status: None,
    }
  }

  #[test]
  fn the_newest_listing_rows_take_the_web_status_goal() {
    // Listing order is `updated_at` then `id`, both newest first: b, c, a, e, f, d, g. The first five take a goal.
    let listed = [("a", 5), ("c", 5), ("b", 9), ("d", 1), ("e", 3), ("f", 2), ("g", 0)];
    let mut rows: Vec<SessionMetadata> = listed.into_iter().map(|(id, second)| row(id, second)).collect();
    give_newest_web_status(&mut rows, &mut Rng::new(1));
    let carrying: Vec<&str> = rows.iter().filter(|row| row.web_status.is_some()).map(|row| row.id.as_str()).collect();
    assert_eq!(carrying, ["b", "c", "a", "e", "f"]);
    let goal = rows[0].web_status.as_ref().expect("the newest row carries a goal")["goal"].as_str().expect("a goal");
    assert_eq!(goal.chars().count(), NEWEST_WEB_STATUS_GOAL_CHARS);
  }

  #[test]
  fn the_default_shape_writes_cron_tasks_that_bind_existing_manager_sessions() {
    // One session generates only a 1.7 KB log so this unit test stays within the 0.1 s limit; multi-session
    // generation (144 MB+) runs in the improve integration suite.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let home = dir.path().join("home");
    generate(&home, 1, 1, Shape::Default, Language::Mixed).expect("the default home");
    let manifest = Manifest::read(&home).expect("the manifest");

    let task_files = fs::read_dir(home.join(cron::CRON_DIR)).expect("the cron directory").count();
    assert_eq!(task_files, manifest.cron_tasks.len(), "the manifest lists every task file");
    assert_eq!(manifest.cron_tasks.len(), 5);
    for task in &manifest.cron_tasks {
      let file = home.join(cron::CRON_DIR).join(format!("{}.yaml", task.name));
      let body = fs::read_to_string(&file).expect("a task file");
      assert!(body.contains(&format!("session_id: {}\n", task.session_id)), "{} binds its session", task.name);
      let metadata =
        fs::read(layout::metadata_path(&home.join(&task.session_id))).expect("the bound session's metadata");
      let metadata: Value = serde_json::from_slice(&metadata).expect("the metadata is JSON");
      assert_eq!(metadata["profile"], "manager", "{} binds a manager", task.name);
    }

    // Two rows repeat to fill the four slots; 40 rows pick the four middle rows of the listing.
    let two: Vec<SessionMetadata> = (0..2).map(|index| row(&format!("s{index}"), index)).collect();
    let two_bound = cron_bound_sessions(&two).expect("two sessions");
    assert_eq!(two_bound, ["s0", "s1", "s0", "s1"]);

    let forty: Vec<SessionMetadata> = (0..40).map(|index| row(&format!("s{index:02}"), index)).collect();
    let forty_bound = cron_bound_sessions(&forty).expect("forty sessions");
    assert_eq!(forty_bound, ["s18", "s19", "s20", "s21"]);
  }
}
