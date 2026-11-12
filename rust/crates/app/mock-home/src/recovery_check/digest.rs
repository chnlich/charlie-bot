//! The recovery check's digests, for a line-by-line diff between two trials of the same seed.
//!
//! [`files`] lists every file that the trial server wrote after the stamp: the bytes it appended to the seed's copy,
//! the JSON keys it changed, or the new file's content. [`items`] counts the server log's events with their
//! identifying fields. Both normalize what differs between two correct trials: timestamps, fresh uuids, process ids,
//! durations, byte counts and the lab's paths.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use charliebot_core::config::SESSIONS_DIR_NAME;
use charliebot_core::event_types::{SERVER_READY_EVENT, STARTUP_SEGMENT_EVENT};
use charliebot_core::home::{
  BOOT_RECOVERY_FILE_NAME, BOOT_SKIP_PROOF_FILE_NAME, BOOT_TRIGGER_INDEX_FILE_NAME, STATE_DIR_NAME, state_rel_path,
};
use charliebot_store::layout;
use regex::{Captures, NoExpand, Regex};
use serde_json::Value;
use walkdir::WalkDir;

static UUID: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").expect("uuid regex"));
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?|\d{8}T\d{6}Z")
    .expect("timestamp regex")
});
/// A duration inside a debug-printed map, as `master_cc_finished` prints its finish extras.
static EMBEDDED_MS: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r#""(\w+_ms)": Number\(\d+\)"#).expect("embedded duration regex"));
static VOLATILE_KEY: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"^(pid|pid_start|.*_ms|.*_ns|.*_bytes|.*_seconds|duration.*|elapsed.*|port|url|rss.*)$")
    .expect("volatile key regex")
});
/// A server log line: time, level, source location, event name and fields.
static LINE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"^\S+ \S+ \[(\w+)\s*\] \S+\s+(\S+)(.*)$").expect("log line regex"));
static FIELD: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r#"(\w+)=('(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*"|\S+)"#).expect("log field regex"));
/// Lines that carry only timing, or that each build words differently by design.
const DROP_EVENTS: [&str; 3] = [STARTUP_SEGMENT_EVENT, "task_tree_recovery_phases", SERVER_READY_EVENT];
/// The startup-only files that are line-oriented rather than one JSON document: the boot's skip proof (a header line,
/// then one row per skipped node; `charliebot_tasks::skip_proof` owns its form), the boot's trigger index (a header
/// line, then one row per trigger file a boot proved not pending; `charliebot_triggers::trigger_index` owns its form)
/// and the fact checkpoint (a header line, a facts line, a binary table and one line per kept event). The JSON arms
/// below cannot read these.
const LINE_ORIENTED_STARTUP_FILES: [&str; 3] =
  [BOOT_SKIP_PROOF_FILE_NAME, BOOT_TRIGGER_INDEX_FILE_NAME, layout::FACT_CHECKPOINT_FILE_NAME];

/// The lab's paths that differ between two trials, with their stand-ins, most specific first.
fn lab_paths(lab: &Path) -> Vec<(Regex, &'static str)> {
  let lab = regex::escape(&lab.display().to_string());
  [
    (format!("{lab}/trials/trial-[0-9a-f]{{12}}"), "<tree>"),
    (format!(r#"{lab}/runs/[^/ '"]+"#), "<run>"),
    (format!(r#"{lab}/bin-[^/ '"]+"#), "<bin>"),
    (format!(r#"{lab}/start/[^/ '"]+"#), "<start>"),
    (r"charliebot-release-trial-serve-[0-9TZ]+-\d+".to_string(), "<serve-unit>"),
  ]
  .into_iter()
  .map(|(pattern, stand_in)| (Regex::new(&pattern).expect("lab path regex"), stand_in))
  .collect()
}

/// Rewrites the volatile parts of one file's or one log's text. A uuid outside `known` is fresh: numbered in order of
/// appearance, or named `<new>` alone when fresh ids interleave in thread order.
struct Names<'a> {
  known: &'a HashSet<String>,
  numbered: bool,
  fresh: HashMap<String, String>,
  paths: &'a [(Regex, &'static str)],
}

impl<'a> Names<'a> {
  fn new(known: &'a HashSet<String>, numbered: bool, paths: &'a [(Regex, &'static str)]) -> Self {
    Names { known, numbered, fresh: HashMap::new(), paths }
  }

  fn text(&mut self, value: &str) -> String {
    let mut text = value.to_string();
    for (pattern, stand_in) in self.paths {
      text = pattern.replace_all(&text, NoExpand(stand_in)).into_owned();
    }
    let text = EMBEDDED_MS.replace_all(&text, r#""${1}": <volatile>"#);
    let text = TIMESTAMP.replace_all(&text, "<ts>");
    UUID.replace_all(&text, |caps: &Captures| self.uuid(&caps[0])).into_owned()
  }

  fn uuid(&mut self, value: &str) -> String {
    if self.known.contains(value) {
      return value.to_string();
    }
    if !self.numbered {
      return "<new>".to_string();
    }
    let next = format!("<new{}>", self.fresh.len());
    self.fresh.entry(value.to_string()).or_insert(next).clone()
  }

  /// The normalized value, with object keys sorted, so its compact JSON is canonical.
  fn value(&mut self, value: &Value, key: Option<&str>) -> Value {
    if key.is_some_and(|key| VOLATILE_KEY.is_match(key)) {
      return Value::String("<volatile>".to_string());
    }
    match value {
      Value::String(text) => Value::String(self.text(text)),
      Value::Array(items) => Value::Array(items.iter().map(|item| self.value(item, None)).collect()),
      Value::Object(map) => {
        let mut entries: Vec<(String, Value)> =
          map.iter().map(|(name, item)| (self.text(name), self.value(item, Some(name)))).collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Value::Object(entries.into_iter().collect())
      }
      other => other.clone(),
    }
  }
}

fn dump(value: &Value) -> String {
  serde_json::to_string(value).expect("a JSON value serializes")
}

fn body_lines(rel: &str, data: &[u8], names: &mut Names) -> Result<Vec<String>> {
  let text = String::from_utf8_lossy(data);
  if rel.ends_with(".json") {
    let value: Value = serde_json::from_str(&text).with_context(|| format!("{rel} is not JSON"))?;
    return Ok(vec![dump(&names.value(&value, None))]);
  }
  if rel.ends_with(".jsonl") || rel.ends_with(".ndjson") {
    return Ok(
      text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| match serde_json::from_str::<Value>(line) {
          Ok(value) => dump(&names.value(&value, None)),
          Err(_) => format!("unparsed {}", names.text(line)),
        })
        .collect(),
    );
  }
  Ok(text.lines().map(|line| names.text(line)).collect())
}

/// The seed's copy of a trial home file: a session file, the boot marker, or a line-oriented startup file.
fn seed_path(seed: &Path, rel: &str) -> Option<std::path::PathBuf> {
  match rel.split_once('/') {
    Some((SESSIONS_DIR_NAME, rest)) => Some(seed.join(rest)),
    _ if rel == state_rel_path(BOOT_RECOVERY_FILE_NAME) => Some(seed.join(BOOT_RECOVERY_FILE_NAME)),
    _ if rel == state_rel_path(BOOT_SKIP_PROOF_FILE_NAME) => Some(seed.join(BOOT_SKIP_PROOF_FILE_NAME)),
    _ if rel == state_rel_path(BOOT_TRIGGER_INDEX_FILE_NAME) => Some(seed.join(BOOT_TRIGGER_INDEX_FILE_NAME)),
    _ => None,
  }
}

/// The ids a trial file may name without being fresh: every seed session's id, and every uuid in the seed's copy of
/// the file's session.
fn session_known(seed: &Path, session_id: &str, session_ids: &HashSet<String>) -> Result<HashSet<String>> {
  let mut known = session_ids.clone();
  let session_dir = seed.join(session_id);
  if !session_dir.exists() {
    return Ok(known);
  }
  for entry in WalkDir::new(&session_dir) {
    let entry = entry?;
    let path_text = entry.path().display().to_string();
    known.extend(UUID.find_iter(&path_text).map(|found| found.as_str().to_string()));
    if entry.file_type().is_file() {
      let data = fs::read(entry.path()).with_context(|| format!("cannot read {}", entry.path().display()))?;
      let text = String::from_utf8_lossy(&data);
      known.extend(UUID.find_iter(&text).map(|found| found.as_str().to_string()));
    }
  }
  Ok(known)
}

fn dir_names(dir: &Path) -> Result<HashSet<String>> {
  fs::read_dir(dir)
    .with_context(|| format!("cannot list {}", dir.display()))?
    .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
    .collect()
}

/// The digest of every file under `home`'s `sessions` and `state` that the trial server wrote after `stamp`.
pub fn files(seed: &Path, home: &Path, stamp: &Path, lab: &Path) -> Result<String> {
  let since = fs::metadata(stamp).with_context(|| format!("cannot stat {}", stamp.display()))?.modified()?;
  let session_ids = dir_names(seed)?;
  let paths = lab_paths(lab);
  let mut changed = Vec::new();
  for top in [SESSIONS_DIR_NAME, STATE_DIR_NAME] {
    for entry in WalkDir::new(home.join(top)) {
      let entry = entry?;
      if !entry.file_type().is_dir() && entry.path().symlink_metadata()?.modified()? > since {
        changed.push(entry.path().strip_prefix(home).expect("a walked path lies under the home").display().to_string());
      }
    }
  }
  let mut known_by_session: HashMap<String, HashSet<String>> = HashMap::new();
  let mut blocks = Vec::new();
  for rel in changed {
    let data = fs::read(home.join(&rel)).with_context(|| format!("cannot read {rel}"))?;
    let known = match rel.split('/').collect::<Vec<_>>()[..] {
      [SESSIONS_DIR_NAME, session_id, ..] => {
        if !known_by_session.contains_key(session_id) {
          known_by_session.insert(session_id.to_string(), session_known(seed, session_id, &session_ids)?);
        }
        &known_by_session[session_id]
      }
      _ => &session_ids,
    };
    let mut names = Names::new(known, true, &paths);
    let shown = names.text(&rel);
    let seed_data = match seed_path(seed, &rel) {
      Some(path) if path.exists() => Some(fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?),
      _ => None,
    };
    // A line-oriented startup file cannot be read by the JSON arms below. One build writes it where the other writes
    // nothing, so the digest names the file and its size.
    if LINE_ORIENTED_STARTUP_FILES.iter().any(|name| rel.ends_with(name)) {
      let kind = if seed_data.is_some() { "rewrite" } else { "new" };
      blocks.push((format!("== {shown} {kind}"), vec![format!("{} bytes", data.len())]));
      continue;
    }
    let (header, lines) = match seed_data {
      None => (format!("== {shown} new"), body_lines(&rel, &data, &mut names)?),
      Some(seed_data) if data.starts_with(&seed_data) && !rel.ends_with(".json") => {
        (format!("== {shown} append"), body_lines(&rel, &data[seed_data.len()..], &mut names)?)
      }
      Some(seed_data) if rel.ends_with(".json") => {
        let before: Value = serde_json::from_slice(&seed_data).with_context(|| format!("seed {rel} is not JSON"))?;
        let after: Value = serde_json::from_slice(&data).with_context(|| format!("{rel} is not JSON"))?;
        let lines = match (&before, &after) {
          (Value::Object(before), Value::Object(after)) => {
            let absent = Value::String("<absent>".to_string());
            let keys: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
            keys
              .into_iter()
              .filter_map(|key| {
                let (old, new) = (before.get(key).unwrap_or(&absent), after.get(key).unwrap_or(&absent));
                (old != new).then(|| {
                  format!("{key}: {} -> {}", dump(&names.value(old, Some(key))), dump(&names.value(new, Some(key))))
                })
              })
              .collect()
          }
          _ => body_lines(&rel, &data, &mut names)?,
        };
        (format!("== {shown} rewrite"), lines)
      }
      Some(_) => (format!("== {shown} rewrite"), body_lines(&rel, &data, &mut names)?),
    };
    blocks.push((header, lines));
  }
  blocks.sort();
  let mut out = String::new();
  for (header, lines) in blocks {
    out.push_str(&header);
    out.push('\n');
    for line in lines {
      out.push_str("  ");
      out.push_str(&line);
      out.push('\n');
    }
  }
  Ok(out)
}

/// Every seed session's id and the ids of its run records.
fn seed_ids(seed: &Path) -> Result<HashSet<String>> {
  let mut known = HashSet::new();
  for entry in fs::read_dir(seed).with_context(|| format!("cannot list {}", seed.display()))? {
    let entry = entry?;
    if !entry.file_type()?.is_dir() {
      continue;
    }
    known.insert(entry.file_name().to_string_lossy().into_owned());
    let runs = layout::runs_dir(&entry.path());
    if runs.is_dir() {
      known.extend(dir_names(&runs)?);
    }
  }
  Ok(known)
}

/// The server log's events with their identifying fields, counted, one `<count> <item>` line per distinct item.
pub fn items(log: &Path, seed: &Path, lab: &Path) -> Result<String> {
  let known = seed_ids(seed)?;
  let paths = lab_paths(lab);
  // Fresh ids interleave in thread order, so an item names each one <new>, and the counts compare them.
  let mut names = Names::new(&known, false, &paths);
  let data = fs::read(log).with_context(|| format!("cannot read {}", log.display()))?;
  let mut counts: BTreeMap<String, usize> = BTreeMap::new();
  for line in String::from_utf8_lossy(&data).lines() {
    let Some(parts) = LINE.captures(line) else { continue };
    let (level, event, rest) = (&parts[1], &parts[2], &parts[3]);
    if DROP_EVENTS.contains(&event) || event.ends_with("_slow") {
      continue;
    }
    let mut item = format!("{level} {event}");
    for field in FIELD.captures_iter(rest) {
      if VOLATILE_KEY.is_match(&field[1]) {
        continue;
      }
      item.push_str(&format!(" {}={}", &field[1], names.text(&field[2])));
    }
    *counts.entry(item).or_default() += 1;
  }
  Ok(counts.into_iter().map(|(item, count)| format!("{count:6} {item}\n")).collect())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Two trials of one seed differ in times, fresh ids, pids, durations and trial paths; the digests hide exactly that.
  #[test]
  fn the_digests_keep_what_a_trial_changed_and_hide_what_two_trials_differ_in() {
    let lab = tempfile::tempdir().expect("lab");
    let seed = lab.path().join("seed");
    let session = "11111111-1111-4111-8111-111111111111";
    let run = "22222222-2222-4222-8222-222222222222";
    fs::create_dir_all(seed.join(session).join("data").join("runs").join(run)).expect("seed dirs");
    let log_line = format!(r#"{{"id":"{run}","type":"user","timestamp":"2026-11-01T10:00:00Z"}}"#);
    fs::write(seed.join(session).join("data").join("chat_events.jsonl"), format!("{log_line}\n")).expect("seed log");
    fs::write(seed.join(session).join("metadata.json"), r#"{"status":"active","pid":7,"name":"n"}"#).expect("meta");
    let stamp = lab.path().join("stamp");
    std::fs::File::create(&stamp)
      .expect("stamp")
      .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
      .expect("set stamp before trial files");

    let digest_of = |fresh: &str, pid: u32| {
      let home = lab.path().join(format!("home-{pid}"));
      let data = home.join(SESSIONS_DIR_NAME).join(session).join("data");
      fs::create_dir_all(&data).expect("home dirs");
      fs::create_dir_all(home.join(STATE_DIR_NAME)).expect("state dir");
      let appended = format!(r#"{{"id":"{fresh}","type":"run_finished","run_id":"{run}","elapsed_ms":{pid}}}"#);
      fs::write(data.join("chat_events.jsonl"), format!("{log_line}\n{appended}\n")).expect("trial log");
      let meta =
        format!(r#"{{"status":"paused","pid":{pid},"name":"n","updated_at":"2026-11-03T0{}:00:00Z"}}"#, pid % 10);
      fs::write(home.join(SESSIONS_DIR_NAME).join(session).join("metadata.json"), meta).expect("trial meta");
      files(&seed, &home, &stamp, lab.path()).expect("files digest")
    };
    let first = digest_of("33333333-3333-4333-8333-333333333333", 101);
    let second = digest_of("44444444-4444-4444-8444-444444444444", 202);
    assert_eq!(first, second);
    assert!(first.contains(&format!("== sessions/{session}/data/chat_events.jsonl append\n")), "{first}");
    assert!(first.contains(&format!(r#""id":"<new0>","run_id":"{run}""#)), "{first}");
    assert!(first.contains(r#"status: "active" -> "paused""#), "{first}");
    assert!(first.contains(r#"updated_at: "<absent>" -> "<ts>""#), "{first}");
    assert!(first.contains(r#"pid: "<volatile>" -> "<volatile>""#), "{first}");
  }

  #[test]
  fn server_log_digest_hides_trial_paths_and_new_run_ids() {
    let lab = tempfile::tempdir().expect("lab");
    let seed = lab.path().join("seed");
    let session = "11111111-1111-4111-8111-111111111111";
    let run = "22222222-2222-4222-8222-222222222222";
    fs::create_dir_all(seed.join(session).join("data").join("runs").join(run)).expect("seed dirs");
    let tree = lab.path().join("trials").join("trial-0123456789ab");
    let log = lab.path().join("server-1.log");
    fs::write(
      &log,
      format!(
        "2026-11-03 17:39:02.881Z [info     ] tasks/recovery.rs:521          task_tree_recovery_phases      x=1\n\
         2026-11-03 17:39:02.900Z [warn     ] tasks/recovery.rs:600          run_followed                   \
         session_id={session} run_id=55555555-5555-4555-8555-555555555555 path={}/a elapsed_ms=4 reason='a b'\n",
        tree.display()
      ),
    )
    .expect("server log");
    let counted = items(&log, &seed, lab.path()).expect("items digest");
    assert_eq!(
      counted,
      format!("     1 warn run_followed session_id={session} run_id=<new> path=<tree>/a reason='a b'\n")
    );
  }
}
