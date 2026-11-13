//! The held remote session: one ssh session per remote host that a pending remote wait watches.
//!
//! The session runs a POSIX sh script on the host. The script checks every watched pid each second and every
//! watched Slurm job each ten seconds, prints `END pid <pid> <epoch.ns>` or `END slurm <jobid> <state>` when a
//! target ends, and prints `HB` every thirty seconds. A waiter subscribes to the ends of its own targets and runs
//! its readback when one arrives. The interval probes in `triggers.rs` keep running alongside, so whichever path
//! reports the end first wakes the trigger, and the fire path is idempotent per trigger.
//!
//! The session holds the ssh child of its host. A change to the host's wait set reopens the session, so the new
//! script checks every target on its first pass and an end inside the reopen gap is still reported. Silence for
//! longer than the silence limit kills the child, and every session that ends reconnects after a pause.
//!
//! The switch `USE_REMOTE_HELD_SESSION` turns the module off: unset or `1` opens sessions, `0` or `false` (any
//! case) leaves every watch inert, and the waiter then runs the interval probes alone.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::end_record;

/// The switch that turns held sessions on or off.
pub const USE_REMOTE_HELD_SESSION: &str = "USE_REMOTE_HELD_SESSION";

/// The remote pid check interval, in seconds.
const PID_CHECK_SECONDS: u64 = 1;
/// The remote Slurm check interval, in seconds.
const SLURM_CHECK_SECONDS: u64 = 10;
/// The heartbeat interval, in seconds.
const HEARTBEAT_SECONDS: u64 = 30;
/// How long a session may stay silent before the server kills it.
const SILENCE_SECONDS: u64 = 90;
/// How long a session waits before it reconnects.
const RECONNECT_PAUSE_SECONDS: u64 = 30;

/// The script one held session runs on the remote host. The `@...@` words are replaced with the wait set and the
/// sacct query before it is sent. The pid and job checks use the same predicates as the interval probes: `kill -0`
/// for a pid, and the terminal-state rule of the sacct query for a job.
const SCRIPT: &str = r##"pids='@PIDS@'
jobs='@JOBS@'
tick=0
while :; do
  if [ $((tick % @HEARTBEAT@)) -eq 0 ]; then echo HB; fi
  rest=''
  for pid in $pids; do
    if kill -0 "$pid" >/dev/null 2>&1; then
      rest="$rest $pid"
    else
      echo "END pid $pid $(date +%s.%N)"
    fi
  done
  pids=$rest
  if [ -n "$jobs" ] && [ $((tick % @SLURM_CHECK@)) -eq 0 ]; then
    rows=$(sacct -j "$jobs" -X -n -P --format=@SACCT_FORMAT@ 2>/dev/null)
    gone=''
    while IFS='|' read -r id state code end; do
      [ -n "$id" ] || continue
      set -- $state
      case "$1" in
        @TERMINAL@) echo "END slurm $id $state"; gone="$gone $id" ;;
      esac
    done <<ROWS
$rows
ROWS
    left=''
    for id in $(echo "$jobs" | tr ',' ' '); do
      case " $gone " in
        *" $id "*) ;;
        *) left="${left:+$left,}$id" ;;
      esac
    done
    jobs=$left
  fi
  sleep @PID_CHECK@
  tick=$((tick + 1))
done
"##;

/// How long a session may stay silent, and how long it waits before it reconnects.
#[derive(Clone, Copy, Debug)]
pub struct HeldTiming {
  pub silence: Duration,
  pub reconnect_pause: Duration,
}

/// The production limits: silence for 90 seconds kills the session, and a reconnect waits 30 seconds.
pub const PRODUCTION_TIMING: HeldTiming = HeldTiming {
  silence: Duration::from_secs(SILENCE_SECONDS),
  reconnect_pause: Duration::from_secs(RECONNECT_PAUSE_SECONDS),
};

/// Whether the switch leaves held sessions on: unset means on, and `0` or `false` (any case) means off.
pub fn enabled_from_env() -> bool {
  enabled_from_value(std::env::var(USE_REMOTE_HELD_SESSION))
}

fn enabled_from_value(value: Result<String, std::env::VarError>) -> bool {
  match value {
    Ok(value) => {
      let value = value.trim();
      value != "0" && !value.eq_ignore_ascii_case("false")
    }
    Err(std::env::VarError::NotPresent | std::env::VarError::NotUnicode(_)) => true,
  }
}

/// One remote target a wait watches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeldTarget {
  Pid { host: String, pid: i64 },
  Slurm { host: String, job_id: i64 },
}

/// One end a remote script reported for a target that a wait watches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeldEnd {
  Pid { host: String, pid: i64 },
  Slurm { host: String, job_id: i64, state: String },
}

/// Builds the argv of one remote command on a host.
pub type SshArgv = Arc<dyn Fn(&str, &str) -> io::Result<Vec<String>> + Send + Sync>;

/// The registry of held sessions. The sessions and their watches share the state through one lock.
pub struct HeldSessions {
  state: Arc<StdMutex<State>>,
}

struct State {
  enabled: bool,
  argv: SshArgv,
  timing: HeldTiming,
  terminal_states: &'static [&'static str],
  next_watch: u64,
  hosts: BTreeMap<String, HostWatch>,
}

#[derive(Default)]
struct HostWatch {
  watchers: Vec<Watcher>,
  session: Option<Session>,
}

/// One open session and the wait set its script was written for.
struct Session {
  keys: WaitSet,
  task: JoinHandle<()>,
}

/// The pids and Slurm jobs one host's script watches.
#[derive(Clone, Default, PartialEq, Eq)]
struct WaitSet {
  pids: BTreeSet<i64>,
  jobs: BTreeSet<i64>,
}

impl WaitSet {
  fn union<'a>(sets: impl Iterator<Item = &'a WaitSet>) -> WaitSet {
    let mut all = WaitSet::default();
    for set in sets {
      all.pids.extend(&set.pids);
      all.jobs.extend(&set.jobs);
    }
    all
  }

  fn wants(&self, end: &HeldEnd) -> bool {
    match end {
      HeldEnd::Pid { pid, .. } => self.pids.contains(pid),
      HeldEnd::Slurm { job_id, .. } => self.jobs.contains(job_id),
    }
  }

  fn remove(&mut self, end: &HeldEnd) {
    match end {
      HeldEnd::Pid { pid, .. } => {
        self.pids.remove(pid);
      }
      HeldEnd::Slurm { job_id, .. } => {
        self.jobs.remove(job_id);
      }
    }
  }

  fn insert(&mut self, target: &HeldTarget) {
    match target {
      HeldTarget::Pid { pid, .. } => {
        self.pids.insert(*pid);
      }
      HeldTarget::Slurm { job_id, .. } => {
        self.jobs.insert(*job_id);
      }
    }
  }
}

/// One wait's targets on one host that no session has reported yet, and the channel its ends go to.
struct Watcher {
  id: u64,
  trigger_id: String,
  set: WaitSet,
  tx: mpsc::UnboundedSender<HeldEnd>,
}

impl HeldSessions {
  /// A registry with the switch's state, the ssh argv builder, the silence and reconnect limits, and the Slurm
  /// terminal states the remote script checks.
  pub fn new(
    enabled: bool,
    argv: SshArgv,
    timing: HeldTiming,
    terminal_states: &'static [&'static str],
  ) -> HeldSessions {
    let state = State { enabled, argv, timing, terminal_states, next_watch: 0, hosts: BTreeMap::new() };
    HeldSessions { state: Arc::new(StdMutex::new(state)) }
  }

  /// Watch `targets` for one trigger. The returned watch yields the ends the remote scripts report for them until it
  /// is dropped; dropping it removes its targets from every host's wait set.
  pub fn watch(&self, trigger_id: &str, targets: Vec<HeldTarget>) -> HeldWatch {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut state = self.state.lock().expect("held session state is not poisoned");
    let mut hosts: BTreeMap<String, WaitSet> = BTreeMap::new();
    if state.enabled {
      for target in targets {
        match target {
          HeldTarget::Pid { host, pid } => {
            hosts.entry(host).or_default().pids.insert(pid);
          }
          HeldTarget::Slurm { host, job_id } => {
            hosts.entry(host).or_default().jobs.insert(job_id);
          }
        }
      }
    }
    state.next_watch += 1;
    let id = state.next_watch;
    for (host, set) in &hosts {
      let watcher = Watcher { id, trigger_id: trigger_id.to_string(), set: set.clone(), tx: tx.clone() };
      state.hosts.entry(host.clone()).or_default().watchers.push(watcher);
      refresh(&self.state, &mut state, host);
    }
    HeldWatch { state: Arc::clone(&self.state), id, hosts: hosts.into_keys().collect(), rx, _tx: tx }
  }
}

/// One wait's subscription to the ends of its targets on every host it names.
pub struct HeldWatch {
  state: Arc<StdMutex<State>>,
  id: u64,
  hosts: Vec<String>,
  rx: mpsc::UnboundedReceiver<HeldEnd>,
  /// The watch keeps its own sender, so the channel stays open when no host registers the watch (the switch is off).
  _tx: mpsc::UnboundedSender<HeldEnd>,
}

impl HeldWatch {
  /// The next end a remote script reported for one of this watch's targets.
  pub async fn recv(&mut self) -> HeldEnd {
    self.rx.recv().await.expect("the watch holds the sender its channel needs")
  }

  /// Watch one target again after a readback found it still alive. The script dropped the target when it reported
  /// that end, and the registry dropped it from this watch's set with the end. Without this call only the
  /// interval probe watches the target. The reopened session checks the target on its first pass.
  pub fn resubscribe(&mut self, target: HeldTarget) {
    let host = match &target {
      HeldTarget::Pid { host, .. } | HeldTarget::Slurm { host, .. } => host.clone(),
    };
    if !self.hosts.contains(&host) {
      return;
    }
    let mut state = self.state.lock().expect("held session state is not poisoned");
    let Some(entry) = state.hosts.get_mut(&host) else {
      return;
    };
    let Some(watcher) = entry.watchers.iter_mut().find(|watcher| watcher.id == self.id) else {
      return;
    };
    watcher.set.insert(&target);
    refresh(&self.state, &mut state, &host);
  }
}

impl Drop for HeldWatch {
  fn drop(&mut self) {
    if self.hosts.is_empty() {
      return;
    }
    let mut state = self.state.lock().expect("held session state is not poisoned");
    for host in &self.hosts {
      if let Some(entry) = state.hosts.get_mut(host) {
        entry.watchers.retain(|watcher| watcher.id != self.id);
      }
    }
    for host in &self.hosts {
      refresh(&self.state, &mut state, host);
    }
  }
}

/// Bring one host's session in line with its watchers. No watcher closes the session; a changed wait set reopens it.
fn refresh(shared: &Arc<StdMutex<State>>, state: &mut State, host: &str) {
  let terminal_states = state.terminal_states;
  let argv = Arc::clone(&state.argv);
  let timing = state.timing;
  let Some(entry) = state.hosts.get_mut(host) else {
    return;
  };
  if entry.watchers.is_empty() {
    if let Some(session) = entry.session.take() {
      session.task.abort();
    }
    state.hosts.remove(host);
    return;
  }
  let keys = WaitSet::union(entry.watchers.iter().map(|watcher| &watcher.set));
  if entry.session.as_ref().is_some_and(|session| session.keys == keys) {
    return;
  }
  if let Some(session) = entry.session.take() {
    session.task.abort();
  }
  // Only a watch dropped after its runtime has stopped gets here, and then no session has a runtime left to run in.
  let Ok(runtime) = tokio::runtime::Handle::try_current() else {
    return;
  };
  let trigger_ids = entry
    .watchers
    .iter()
    .map(|watcher| watcher.trigger_id.as_str())
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect::<Vec<_>>();
  let script = held_script(&keys, terminal_states);
  let task =
    runtime.spawn(run_session(Arc::clone(shared), host.to_string(), script, trigger_ids.join(","), argv, timing));
  entry.session = Some(Session { keys, task });
}

/// The script text one wait set needs: the pids and jobs it watches, the terminal states, and the query's columns.
fn held_script(set: &WaitSet, terminal_states: &[&str]) -> String {
  let pids = set.pids.iter().map(i64::to_string).collect::<Vec<_>>().join(" ");
  let jobs = set.jobs.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
  SCRIPT
    .replace("@PIDS@", &pids)
    .replace("@JOBS@", &jobs)
    .replace("@TERMINAL@", &terminal_states.join("|"))
    .replace("@SACCT_FORMAT@", &end_record::SACCT_FORMAT_COLUMNS.join(","))
    .replace("@PID_CHECK@", &PID_CHECK_SECONDS.to_string())
    .replace("@SLURM_CHECK@", &SLURM_CHECK_SECONDS.to_string())
    .replace("@HEARTBEAT@", &HEARTBEAT_SECONDS.to_string())
}

/// One host's session for its whole life: it reconnects after every failure, and it ends only when its task is
/// aborted by a change to the wait set.
async fn run_session(
  shared: Arc<StdMutex<State>>,
  host: String,
  script: String,
  trigger_ids: String,
  argv: SshArgv,
  timing: HeldTiming,
) {
  loop {
    let error = run_once(&shared, &host, &script, &trigger_ids, &argv, timing.silence).await;
    tracing::error!(
      target: "charliebot_triggers",
      host = %host,
      trigger_ids = %trigger_ids,
      error = %error,
      "held_session_error"
    );
    sleep(timing.reconnect_pause).await;
  }
}

/// Make the kernel SIGKILL `command`'s child when this server exits, so no held ssh client outlives the server.
///
/// A client that outlived the server would keep its ssh connection, and its command line names the server's
/// ControlPath, until the remote script ends on its heartbeat. `PR_SET_PDEATHSIG` follows the thread that starts the
/// child, and the sessions start their clients on runtime worker threads, which live as long as the server. The
/// parent-death signal is Linux's; elsewhere the child still refuses to start once the server is gone, and an
/// orphan ends when its channel closes.
fn die_with_server(command: &mut std::process::Command) {
  let server = std::process::id() as libc::pid_t;
  // SAFETY: prctl and getppid are async-signal-safe, which is all a hook between fork and exec may call.
  unsafe {
    command.pre_exec(move || {
      #[cfg(target_os = "linux")]
      if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
        return Err(io::Error::last_os_error());
      }
      // The kernel signals only a child whose parent is alive when prctl runs. A server that exited first has left
      // the child reparented, so the child does not start.
      if libc::getppid() != server {
        return Err(io::Error::other("the server exited before the held ssh client started"));
      }
      Ok(())
    });
  }
}

/// One ssh session from its start to its end. It returns the reason the session ended.
async fn run_once(
  shared: &StdMutex<State>,
  host: &str,
  script: &str,
  trigger_ids: &str,
  argv: &SshArgv,
  silence: Duration,
) -> String {
  let remote = match shlex::try_quote(script) {
    Ok(quoted) => format!("sh -c {quoted}"),
    Err(error) => return format!("the held script is not quotable: {error}"),
  };
  let command = match argv(host, &remote) {
    Ok(command) => command,
    Err(error) => return format!("building the ssh command failed: {error}"),
  };
  tracing::info!(
    target: "charliebot_triggers",
    trigger_ids = %trigger_ids,
    host = %host,
    purpose = "held",
    "remote_ssh_started"
  );
  let (program, args) = command.split_first().expect("an ssh command names a program");
  let mut process = Command::new(program);
  process.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
  die_with_server(process.as_std_mut());
  let mut child = match process.spawn() {
    Ok(child) => child,
    Err(error) => return format!("spawn failed: {error}"),
  };
  // The last stderr line names the failure ssh printed. A reader task keeps it and is never awaited. A script that
  // outlives its killed session keeps the stderr pipe open, so the session must not wait for the pipe to close.
  let last_stderr = Arc::new(StdMutex::new(None::<String>));
  let stderr_lines = BufReader::new(child.stderr.take().expect("stderr is piped")).lines();
  let sink = Arc::clone(&last_stderr);
  tokio::spawn(async move {
    let mut stderr_lines = stderr_lines;
    loop {
      match stderr_lines.next_line().await {
        Ok(Some(line)) => *sink.lock().expect("stderr sink is not poisoned") = Some(line),
        Ok(None) => break,
        Err(error) => {
          tracing::warn!(target: "charliebot_triggers", error = %error, "held_session_stderr_failed");
          break;
        }
      }
    }
  });
  let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();
  let reason = loop {
    match timeout(silence, lines.next_line()).await {
      Err(_) => break format!("silent for {silence:?}"),
      Ok(Err(error)) => break format!("read failed: {error}"),
      Ok(Ok(None)) => break "the session closed".to_string(),
      Ok(Ok(Some(line))) => {
        if let Err(error) = dispatch_line(shared, host, &line) {
          break error;
        }
      }
    }
  };
  if let Err(error) = child.kill().await {
    tracing::warn!(target: "charliebot_triggers", host = %host, error = %error, "held_session_kill_failed");
  }
  let stderr_line = last_stderr.lock().expect("stderr sink is not poisoned").take();
  match stderr_line {
    Some(line) => format!("{reason}; last stderr line: {line}"),
    None => reason,
  }
}

/// Route one line the script printed. A heartbeat is only proof of life; an end goes to the watches that name it.
fn dispatch_line(shared: &StdMutex<State>, host: &str, line: &str) -> Result<(), String> {
  let words = line.split_whitespace().collect::<Vec<_>>();
  match words.as_slice() {
    ["HB"] => {}
    ["END", "pid", pid, _seen_at] => {
      let pid = pid.parse::<i64>().map_err(|error| format!("malformed END pid line {line:?}: {error}"))?;
      dispatch(shared, host, HeldEnd::Pid { host: host.to_string(), pid });
    }
    ["END", "slurm", job_id, state @ ..] if !state.is_empty() => {
      let job_id = job_id.parse::<i64>().map_err(|error| format!("malformed END slurm line {line:?}: {error}"))?;
      dispatch(shared, host, HeldEnd::Slurm { host: host.to_string(), job_id, state: state.join(" ") });
    }
    _ => tracing::debug!(target: "charliebot_triggers", host = %host, line = %line, "held_session_unknown_line"),
  }
  Ok(())
}

fn dispatch(shared: &StdMutex<State>, host: &str, end: HeldEnd) {
  let mut state = shared.lock().expect("held session state is not poisoned");
  let Some(entry) = state.hosts.get_mut(host) else {
    return;
  };
  // The script drops a target once it reports the end, and a waiter that received the end no longer needs it. Both
  // sets drop the target, so the session stays reusable only while it still watches every target its waits need. A
  // watch that arrives after the end therefore reopens the session, and the new script's first pass reports the end.
  if let Some(session) = entry.session.as_mut() {
    session.keys.remove(&end);
  }
  for watcher in &mut entry.watchers {
    if !watcher.set.wants(&end) {
      continue;
    }
    watcher.set.remove(&end);
    if watcher.tx.send(end.clone()).is_err() {
      tracing::debug!(target: "charliebot_triggers", trigger_id = %watcher.trigger_id, "held_watch_closed");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn switch_is_on_unless_it_reads_zero_or_false() {
    assert!(enabled_from_value(Err(std::env::VarError::NotPresent)));
    assert!(enabled_from_value(Ok("1".to_string())));
    assert!(!enabled_from_value(Ok("0".to_string())));
    assert!(!enabled_from_value(Ok(" False ".to_string())));
  }
}
