//! The recovery check's sessions, added to a copy of a task-tree home. Each holds one case that the task-tree home
//! lacks: an open worker or manager whose newest run is live, dead with its result written, dead mid-run, or landed
//! without its end metadata, and a closed worker whose run is still live. A live run gets a fresh process for each
//! trial through [`attach_live_run`].

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use charliebot_core::event_types::{INPUT_EVENT_IDS, RUN_FINISHED, TASK_CLOSED, TASK_CREATED};
use charliebot_store::control_events::TASK_SPEC_HASH_FIELD;
use charliebot_store::layout;
use charliebot_store::metadata::{
  BASE_BRANCH_FIELD, HAS_UNREAD, KEEP_WORKTREE_FIELD, REPO_PATH_FIELD, SCHEMA_VERSION_FIELD, TASK_TYPE_FIELD,
};
use charliebot_store::run_records::{
  OBSERVATION_BASE_BRANCH, OBSERVATION_BRANCH_NAME, OBSERVATION_EVENTS_REF, OBSERVATION_MODEL,
  OBSERVATION_NATIVE_SESSION_ID, OBSERVATION_PROMPT_SNAPSHOT_REF, OBSERVATION_RAW_LOG_REF, OBSERVATION_REPO_PATH,
  OBSERVATION_RESULT_REF, OBSERVATION_WORKTREE_PATH, RAW_LOG_NAME, RUN_METADATA_NAME, RunKind,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The namespace of every id that [`seed`] writes, so one seed gives the same ids on every run.
const NAMESPACE: Uuid = Uuid::from_u128(0x6f1c2d3e_4b5a_4c7d_8e9f_0a1b2c3d4e5f);
const BACKEND: &str = "charlie-code-release-trial";
const MODEL: &str = "openai/glm-5.3-flash";
/// At or above Linux's PID_MAX_LIMIT (2^22): no process holds it, so the run is dead.
const DEAD_PID: usize = 4_194_304 + 900_000;
/// The pid of a live run until [`attach_live_run`] writes the trial's process.
const LIVE_PLACEHOLDER_PID: usize = 1;

#[derive(Clone, Copy, PartialEq)]
enum Profile {
  Worker,
  Manager,
}

#[derive(Clone, Copy, PartialEq)]
enum RunState {
  /// A process holds the run when the server starts.
  Live,
  /// No process holds the run, and no `run_finished` names it.
  Dead,
  /// A `run_finished` names the run, and its record has no end.
  Landed,
}

struct Case {
  label: &'static str,
  profile: Profile,
  state: RunState,
  /// The run's raw log ends with a result.
  finished: bool,
  /// The task is closed.
  closed: bool,
}

const CASES: [Case; 9] = [
  Case {
    label: "worker-live-finishes",
    profile: Profile::Worker,
    state: RunState::Live,
    finished: true,
    closed: false,
  },
  Case {
    label: "worker-live-crashes",
    profile: Profile::Worker,
    state: RunState::Live,
    finished: false,
    closed: false,
  },
  Case {
    label: "worker-dead-finished",
    profile: Profile::Worker,
    state: RunState::Dead,
    finished: true,
    closed: false,
  },
  Case {
    label: "worker-dead-mid-run",
    profile: Profile::Worker,
    state: RunState::Dead,
    finished: false,
    closed: false,
  },
  Case {
    label: "worker-landed-no-end",
    profile: Profile::Worker,
    state: RunState::Landed,
    finished: true,
    closed: false,
  },
  Case { label: "worker-closed-live", profile: Profile::Worker, state: RunState::Live, finished: true, closed: true },
  Case {
    label: "manager-live-finishes",
    profile: Profile::Manager,
    state: RunState::Live,
    finished: true,
    closed: false,
  },
  Case {
    label: "manager-dead-finished",
    profile: Profile::Manager,
    state: RunState::Dead,
    finished: true,
    closed: false,
  },
  Case {
    label: "manager-dead-mid-run",
    profile: Profile::Manager,
    state: RunState::Dead,
    finished: false,
    closed: false,
  },
];

fn uid(label: &str) -> String {
  Uuid::new_v5(&NAMESPACE, label.as_bytes()).to_string()
}

fn stamp(index: usize, second: usize) -> String {
  format!("2026-11-01T10:{index:02}:{second:02}")
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
  fs::create_dir_all(path.parent().expect("a file path has a parent"))?;
  let mut text = serde_json::to_string_pretty(value)?;
  text.push('\n');
  fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

fn write_lines(path: &Path, values: &[Value]) -> Result<()> {
  fs::create_dir_all(path.parent().expect("a file path has a parent"))?;
  let mut text = String::new();
  for value in values {
    text.push_str(&serde_json::to_string(value)?);
    text.push('\n');
  }
  fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

fn session_meta(session_id: &str, case: &Case, index: usize) -> Value {
  let worker = case.profile == Profile::Worker;
  json!({
    "id": session_id,
    "name": case.label,
    "status": "active",
    "profile": if worker { "worker" } else { "manager" },
    "backend": BACKEND,
    "created_at": format!("{}.000000Z", stamp(index, 0)),
    "updated_at": format!("{}.000000Z", stamp(index, 9)),
    (SCHEMA_VERSION_FIELD): 2,
    (HAS_UNREAD): false,
    "starred": false,
    "task_parent_id": null,
    "task": {
      "goal": format!("Recovery check: {}", case.label),
      "acceptance": if worker { json!(["The recovery check passes."]) } else { json!([]) },
      "context_refs": [],
      (REPO_PATH_FIELD): null,
      (BASE_BRANCH_FIELD): null,
      (TASK_TYPE_FIELD): null,
      (KEEP_WORKTREE_FIELD): false,
      "cold_read": null,
    },
  })
}

fn control(label: &str, kind: &str, session_id: &str, at: &str, actor: &str, fields: Value) -> Value {
  let mut event = Map::new();
  event.insert("id".into(), json!(uid(label)));
  event.insert("type".into(), json!(kind));
  event.insert("timestamp".into(), json!(format!("{at}+00:00")));
  event.insert("actor".into(), json!(actor));
  event.insert("source_session_id".into(), json!(session_id));
  let Value::Object(fields) = fields else { unreachable!("control fields are an object") };
  event.extend(fields);
  Value::Object(event)
}

fn run_record(run: RunRecord) -> Value {
  json!({
    "id": run.run_id,
    "session_id": run.session_id,
    "kind": run.kind,
    (INPUT_EVENT_IDS): run.inputs,
    (TASK_SPEC_HASH_FIELD): null,
    "task_spec_ref": null,
    (OBSERVATION_PROMPT_SNAPSHOT_REF): null,
    "retry_of_run_id": null,
    "review_of_run_id": null,
    "backend": BACKEND,
    (OBSERVATION_MODEL): MODEL,
    (OBSERVATION_NATIVE_SESSION_ID): null,
    "pid": run.pid,
    "pid_start": "12345",
    "started_at": format!("{}Z", run.started_at),
    "ended_at": run.ended_at.as_ref().map(|at| format!("{at}Z")),
    "exit_code": run.ended_at.as_ref().map(|_| 0),
    (OBSERVATION_REPO_PATH): null,
    (OBSERVATION_BASE_BRANCH): null,
    (OBSERVATION_BRANCH_NAME): null,
    (OBSERVATION_WORKTREE_PATH): null,
    "sequence_ref": null,
    (OBSERVATION_RAW_LOG_REF): null,
    (OBSERVATION_EVENTS_REF): null,
    (OBSERVATION_RESULT_REF): null,
  })
}

struct RunRecord<'a> {
  session_id: &'a str,
  run_id: &'a str,
  kind: RunKind,
  inputs: Vec<String>,
  pid: usize,
  started_at: String,
  ended_at: Option<String>,
}

fn raw_log(label: &str, finished: bool) -> Vec<Value> {
  let mut lines = vec![
    json!({"type": "session", "session_id": uid(&format!("{label}/native"))}),
    json!({"type": "thought", "text": format!("Working on {label}.")}),
  ];
  if finished {
    lines.push(json!({"type": "result", "completed": true, "usage": {"input_tokens": 10, "output_tokens": 5}}));
  }
  lines
}

/// Adds the check sessions to `sessions`, a copy of a task-tree home's session directory, and writes one line
/// `<session id> <run id> <label>` per live run to `live_runs`.
pub fn seed(sessions: &Path, live_runs: &Path) -> Result<()> {
  let mut live = String::new();
  for (index, case) in CASES.iter().enumerate() {
    let label = case.label;
    let session_id = uid(label);
    let session_dir = sessions.join(&session_id);
    if session_dir.exists() {
      bail!("{} exists; the seed already holds the check sessions", session_dir.display());
    }
    write_json(&layout::metadata_path(&session_dir), &session_meta(&session_id, case, index))?;
    let actor = if case.profile == Profile::Manager { "user" } else { "agent" };
    let mut events = vec![control(
      &format!("{label}/created"),
      TASK_CREATED,
      &session_id,
      &stamp(index, 0),
      actor,
      json!({
        "request_id": uid(&format!("{label}/request")),
        "task_parent_id": null,
        (TASK_SPEC_HASH_FIELD): null
      }),
    )];
    let runs_dir = layout::runs_dir(&session_dir);
    let (run_id, kind) = match case.profile {
      Profile::Manager => {
        // A finished first turn confirms the user's input, so the open manager has no input waiting for a launch.
        let message_id = uid(&format!("{label}/message"));
        events.push(json!({
          "id": message_id,
          "type": "user",
          "content": format!("Check {label}."),
          "timestamp": format!("{}Z", stamp(index, 1)),
        }));
        let first_run = uid(&format!("{label}/turn-1"));
        events.push(control(
          &format!("{label}/turn-1-finished"),
          RUN_FINISHED,
          &session_id,
          &stamp(index, 3),
          "system",
          json!({"run_id": first_run, (INPUT_EVENT_IDS): [message_id], "outcome": "success"}),
        ));
        let record = run_record(RunRecord {
          session_id: &session_id,
          run_id: &first_run,
          kind: RunKind::ManagerTurn,
          inputs: vec![message_id],
          pid: DEAD_PID,
          started_at: stamp(index, 2),
          ended_at: Some(stamp(index, 3)),
        });
        write_json(&runs_dir.join(&first_run).join(RUN_METADATA_NAME), &record)?;
        write_lines(&runs_dir.join(&first_run).join(RAW_LOG_NAME), &raw_log(&format!("{label}/turn-1"), true))?;
        (uid(&format!("{label}/turn-2")), RunKind::ManagerTurn)
      }
      Profile::Worker => (uid(&format!("{label}/work")), RunKind::Work),
    };
    if case.state == RunState::Landed {
      events.push(control(
        &format!("{label}/finished"),
        RUN_FINISHED,
        &session_id,
        &stamp(index, 6),
        "system",
        json!({"run_id": run_id, (INPUT_EVENT_IDS): [], "outcome": "success"}),
      ));
    }
    if case.closed {
      events.push(control(
        &format!("{label}/closed"),
        TASK_CLOSED,
        &session_id,
        &stamp(index, 7),
        "agent",
        json!({
          "request_id": uid(&format!("{label}/close")),
          "outcome": "completed",
          "summary": "Closed while its run was live.",
          "result_refs": [],
          "run_ids": [run_id],
          "report_to": null,
        }),
      ));
    }
    let pid = if case.state == RunState::Live { LIVE_PLACEHOLDER_PID } else { DEAD_PID + index };
    let record = run_record(RunRecord {
      session_id: &session_id,
      run_id: &run_id,
      kind,
      inputs: Vec::new(),
      pid,
      started_at: stamp(index, 5),
      ended_at: None,
    });
    write_json(&runs_dir.join(&run_id).join(RUN_METADATA_NAME), &record)?;
    write_lines(&runs_dir.join(&run_id).join(RAW_LOG_NAME), &raw_log(label, case.finished))?;
    write_lines(&layout::chat_events_path(&session_dir), &events)?;
    if case.state == RunState::Live {
      live.push_str(&format!("{session_id} {run_id} {label}\n"));
    }
    println!("{label}: session {session_id} run {run_id}");
  }
  fs::write(live_runs, live).with_context(|| format!("cannot write {}", live_runs.display()))
}

/// One line of the live-runs file that [`seed`] writes.
struct LiveRun<'a> {
  session_id: &'a str,
  run_id: &'a str,
  case: &'static Case,
}

fn live_runs(text: &str) -> Result<Vec<LiveRun<'_>>> {
  text
    .lines()
    .map(|line| {
      let fields: Vec<&str> = line.split(' ').collect();
      let [session_id, run_id, label] = fields[..] else { bail!("a live run is `<session> <run> <label>`: {line}") };
      let case = CASES.iter().find(|case| case.label == label).with_context(|| format!("no check case {label}"))?;
      Ok(LiveRun { session_id, run_id, case })
    })
    .collect()
}

fn record_path(sessions: &Path, session_id: &str, run_id: &str) -> std::path::PathBuf {
  layout::runs_dir(&sessions.join(session_id)).join(run_id).join(RUN_METADATA_NAME)
}

/// Writes the process `pid` into the live run's record in `sessions`: its pid, its start time from `/proc` and a fresh
/// `started_at`, so the startup walk finds the run alive.
pub fn attach_live_run(sessions: &Path, session_id: &str, run_id: &str, pid: u32) -> Result<()> {
  let stat_path = format!("/proc/{pid}/stat");
  let stat = fs::read_to_string(&stat_path).with_context(|| format!("cannot read {stat_path}"))?;
  // The fields after the parenthesized command name start at field 3; the start time is field 22.
  let after_name = stat.rsplit_once(')').with_context(|| format!("{stat_path} has no command name"))?.1;
  let pid_start =
    after_name.split_whitespace().nth(22 - 3).with_context(|| format!("{stat_path} has no start time"))?;
  let path = record_path(sessions, session_id, run_id);
  let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
  let mut record: Value = serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
  record["pid"] = json!(pid);
  record["pid_start"] = json!(pid_start);
  record["started_at"] = json!(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
  write_json(&path, &record)
}

/// The live runs in `live_runs` whose task is open and whose record in `sessions` has no `ended_at` yet. A closed
/// task's live run gets a stop request and no follow, so only an open task's run ends after its process does.
pub fn unended_live_runs(sessions: &Path, live_runs_path: &Path) -> Result<usize> {
  let text = fs::read_to_string(live_runs_path).with_context(|| format!("cannot read {}", live_runs_path.display()))?;
  let mut unended = 0;
  for run in live_runs(&text)? {
    if run.case.closed {
      continue;
    }
    let path = record_path(sessions, run.session_id, run.run_id);
    let record: Value =
      serde_json::from_str(&fs::read_to_string(&path)?).with_context(|| format!("{} is not JSON", path.display()))?;
    if record["ended_at"].is_null() {
      unended += 1;
    }
  }
  Ok(unended)
}
