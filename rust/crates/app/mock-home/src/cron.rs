//! The scheduled-task fixture of the default and task-tree shapes: five task files in `.cron.d`, each bound to one of
//! `BOUND_SESSIONS` manager sessions of the home. `write_fixture` is the one writer of the fixture, so both shapes serve
//! the same tasks.
//!
//! Every schedule is midnight on 1 January. A task with no run record first fires at its next such midnight, so no
//! round sees a fire. Each prompt path names a file of the repo, which the trial's checkout holds.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::manifest::ManifestCronTask;

/// The task files beside the sessions. Its dot keeps it out of the seed copy, which takes only visible directories.
pub const CRON_DIR: &str = ".cron.d";
/// The manager sessions the fixture binds. Its tasks name them by index.
pub const BOUND_SESSIONS: usize = 4;
const YEARLY_SCHEDULE: &str = "0 0 1 1 *";

/// One fixture task: its name, the index of its bound session, and its body after the schedule and session lines. The
/// two tasks on session 0 show a session bound to two tasks, one of them disabled. The others cover each source shape
/// the Task & context dialog renders: a master prompt, a default-mode prompt, steps and a handler.
const FIXTURE: [FixtureTask; 5] = [
  FixtureTask {
    name: "mock-yearly-review",
    session: 0,
    body: "prompt_file: prompts/cron/failure_triage.md\nmode: master\nenabled: true\n",
  },
  FixtureTask {
    name: "mock-paused-review",
    session: 0,
    body: "prompt_file: prompts/cron/failure_triage.md\nmode: master\nenabled: false\n",
  },
  FixtureTask {
    name: "mock-default-review",
    session: 1,
    body: "prompt_file: prompts/cron/session/round.md\nenabled: true\n",
  },
  FixtureTask {
    name: "mock-steps-review",
    session: 2,
    body: "steps:\n  - name: selector\n    prompt_file: prompts/cron/memory_curator/memory_selector.md\n  \
           - name: reviewer\n    prompt_file: prompts/cron/memory_curator/memory_reviewer.md\nenabled: true\n",
  },
  FixtureTask { name: "mock-handler-review", session: 3, body: "handler: cool_storage\nenabled: true\n" },
];

struct FixtureTask {
  name: &'static str,
  session: usize,
  body: &'static str,
}

/// Writes the fixture's task files into `home`'s `CRON_DIR`, each bound to its session of `sessions`, and returns the
/// tasks that the manifest lists.
pub fn write_fixture(home: &Path, sessions: &[String; BOUND_SESSIONS]) -> Result<Vec<ManifestCronTask>> {
  let dir = home.join(CRON_DIR);
  fs::create_dir(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
  let mut tasks = Vec::with_capacity(FIXTURE.len());
  for task in &FIXTURE {
    let session_id = &sessions[task.session];
    let body = format!("cron: \"{YEARLY_SCHEDULE}\"\n{}session_id: {session_id}\n", task.body);
    let path = dir.join(format!("{}.yaml", task.name));
    fs::write(&path, body).with_context(|| format!("cannot write {}", path.display()))?;
    tasks.push(ManifestCronTask { name: task.name.to_string(), session_id: session_id.clone() });
  }
  Ok(tasks)
}
