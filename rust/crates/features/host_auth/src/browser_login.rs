//! Browser login: one click starts one login process, shows its approval link, and holds it to an outcome.
//!
//! Vocabulary:
//!
//! - A *login* is one `host_auth.browser_logins` entry.
//! - An *attempt* is one login process started by one click, from the click until its outcome is recorded. The check
//!   that follows an approval is part of the attempt.
//! - A *readback* is a login's check command. It exits 0 while the login is valid. A login bound to a host checks with
//!   the single-host probe instead, so its readback is that probe.
//!
//! Only [`start_login`] and [`probe_link`] start a login command. [`probe_link`] ends its command as soon as the link
//! prints or its timeout passes, and never runs a readback or records an approval. The status table otherwise runs the
//! checks alone, through [`check_readback`] and the host probe. An ended attempt is recorded in `host_auth.json` by
//! `login_state`. A live attempt lives in memory, in the one map of live attempts keyed by login name.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use charliebot_core::proc_status;
use regex::Regex;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, Interest, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::watch;

use crate::config::BrowserLoginConfig;
use crate::host_auth;
use crate::login_state::{self, AttemptRecord, LinkMatch, Outcome};
use crate::process_tree::{self, ROUTE_ERROR_PREFIX};
use crate::timestamps::{self, isoformat};
use time::OffsetDateTime;

/// The approval link must print within this many seconds of the click. The AWS CLI prints it after at most two
/// requests, so a longer wait means the network or the config is broken.
const START_TIMEOUT_SEC: u64 = 30;
/// An AWS device code expires after 600 seconds. This limit only bounds a wedged child process.
const HOLD_LIMIT_SEC: u64 = 30 * 60;
const READBACK_TIMEOUT_SEC: u64 = 30;
const OUTPUT_KEEP_LINES: usize = 200;
const START_FAILURE_TAIL_LINES: usize = 20;
/// The status table shows, and the state file keeps, at most this many characters of a readback's output.
pub const READBACK_SHOWN_CHARS: usize = 300;

/// The 502 body of a start whose driver died before the link or the failure was known.
const ABANDONED_START_BODY: &str = "the browser login ended before it gave an approval link";

/// Where an attempt in progress stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
  Starting,
  WaitingForApproval,
  ReadingBack,
}

impl Phase {
  /// The page's word for this phase.
  pub fn label(self) -> &'static str {
    match self {
      Phase::Starting => "starting",
      Phase::WaitingForApproval => "waiting for approval",
      Phase::ReadingBack => "reading back",
    }
  }
}

/// How a readback ended: its exit code and merged output, the timeout that ended its process tree, or the reason it
/// could not run at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readback {
  Exited { code: i32, output: String },
  TimedOut { after_sec: u64 },
  CannotRun { reason: String },
}

/// The text a readback shows once its timeout has ended it.
pub fn timeout_text(after_sec: u64) -> String {
  format!("readback timed out after {after_sec} s; its process tree was ended")
}

/// A start that produced no approval link. The string is the 502 body of the click that waited on it.
#[derive(Debug, Clone)]
pub struct StartFailed(pub String);

/// Why a value was not given to a login.
#[derive(Debug)]
pub enum InputRefused {
  /// The login is not waiting for input: no attempt is waiting for approval, it has no `input_prompt`, or its value
  /// was already given.
  NotAwaiting,
  /// The login's input could not be written.
  Write(io::Error),
}

/// The state the link watch of one attempt carries.
#[derive(Debug, Clone)]
enum LinkState {
  /// The start is still producing the link.
  Pending,
  /// The link was found. Every joined click redirects to it.
  Link(String),
  /// The start gave no link. The string is the 502 body of the clicks that waited on it.
  Failed(String),
}

/// `user_code_of`: the value of the link's first `user_code=` parameter, in the query or the fragment. The value is
/// empty when the link carries no user code.
pub fn user_code_of(url: &str) -> &str {
  static USER_CODE_PARAM: OnceLock<Regex> = OnceLock::new();
  let pattern = USER_CODE_PARAM
    .get_or_init(|| Regex::new(r"[?&]user_code=([^&#]*)").expect("the built-in user-code pattern compiles"));
  pattern.captures(url).and_then(|captures| captures.get(1)).map(|matched| matched.as_str()).unwrap_or("")
}

/// `missing_path`: the program or the script of an argv that has no file behind it. The program is the first argument,
/// and the script is the first absolute path after it. Later absolute paths are data that a login may create, such as
/// a token cache, so they never count. A bare program name is left to the spawn, which finds it on `PATH`.
pub fn missing_path(argv: &[String]) -> Option<&str> {
  let (program, arguments) = argv.split_first()?;
  let script = arguments.iter().map(String::as_str).find(|arg| arg.starts_with('/'));
  std::iter::once(program.as_str()).chain(script).find(|path| path.starts_with('/') && !Path::new(path).exists())
}

/// The files an attempt reads and writes: the host-auth state, and the ssh config that names the hosts its probe checks.
#[derive(Clone)]
struct AttemptPaths {
  state: PathBuf,
  ssh_config: PathBuf,
}

/// The stdin a login reads its input from, held until the input value is given.
enum InputSink {
  Pipe(ChildStdin),
  Terminal(Arc<PtyMaster>),
}

/// The record one attempt keeps. The attempt's driver task is the only writer, and every access takes the mutex, so a
/// status read never sees a half-written record.
struct AttemptCore {
  login: BrowserLoginConfig,
  paths: AttemptPaths,
  started_at: OffsetDateTime,
  phase: Phase,
  approval_url: Option<String>,
  output: VecDeque<String>,
  login_exit: Option<i32>,
  readback_exit: Option<i32>,
  readback_output: String,
  outcome: Option<Outcome>,
  finished_at: Option<OffsetDateTime>,
  /// The input sink of a login with an `input_prompt`, open until its value is given or the attempt ends.
  input: Option<InputSink>,
  /// Whether this attempt accepted a value. Its output is not kept because the login may echo the value.
  input_submitted: bool,
  /// The live child's pid: the login process, then the readback process. A kill from outside the driver — a click
  /// that replaces the attempt, or the server stop — signals it.
  child_pid: Option<u32>,
  /// The link watch of this start: the approval link, or the 502 body. Every click of this start awaits it.
  link: watch::Sender<LinkState>,
  approval_url_matcher: Regex,
  user_code_matcher: Option<Regex>,
}

impl AttemptCore {
  fn new(login: &BrowserLoginConfig, link: watch::Sender<LinkState>, paths: AttemptPaths) -> Self {
    AttemptCore {
      login: login.clone(),
      paths,
      started_at: timestamps::utc_now(),
      phase: Phase::Starting,
      approval_url: None,
      output: VecDeque::new(),
      login_exit: None,
      readback_exit: None,
      readback_output: String::new(),
      outcome: None,
      finished_at: None,
      input: None,
      input_submitted: false,
      child_pid: None,
      link,
      approval_url_matcher: Regex::new(&login.approval_url_pattern)
        .expect("the config validated the approval-url pattern"),
      user_code_matcher: login
        .user_code_pattern
        .as_deref()
        .map(|pattern| Regex::new(pattern).expect("the config validated the user-code pattern")),
    }
  }

  /// The recorded pid, signaled from outside the driver. An unsignalable pid is a child the driver's own wait already
  /// reaped.
  fn kill_child(&mut self) {
    if let Some(pid) = self.child_pid.take() {
      // SAFETY: a signal send touches only the kernel's process table.
      let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
      if sent != 0 {
        tracing::debug!(pid, "browser_login_child_already_gone");
      }
    }
  }

  /// The attempt's record as the state file keeps it. It exists once the attempt has an outcome.
  fn record(&self) -> Option<AttemptRecord> {
    let (last_line, readback_output) = if self.input_submitted {
      (String::new(), String::new())
    } else {
      (last_line(&self.output), shown_output(&self.readback_output))
    };
    Some(AttemptRecord {
      outcome: self.outcome?,
      started_at: isoformat(self.started_at),
      finished_at: self.finished_at.map(isoformat),
      last_line,
      readback_exit: self.readback_exit,
      readback_output,
    })
  }
}

/// The output a status row shows: the text trimmed and cut to `READBACK_SHOWN_CHARS` characters.
pub fn shown_output(text: &str) -> String {
  text.trim().chars().take(READBACK_SHOWN_CHARS).collect()
}

/// The last output line that is not blank, or an empty string.
fn last_line(output: &VecDeque<String>) -> String {
  output.iter().rev().find(|line| !line.trim().is_empty()).cloned().unwrap_or_default()
}

/// A read-only copy of a live attempt's fields, taken under the record's mutex.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveAttempt {
  pub phase: Phase,
  pub approval_url: Option<String>,
  pub started_at: OffsetDateTime,
  /// Whether the attempt waits for a value that the row's input box sends.
  pub input_open: bool,
}

impl LiveAttempt {
  /// The device code the approval link carries. It is empty before the link is known.
  pub fn user_code(&self) -> &str {
    self.approval_url.as_deref().map(user_code_of).unwrap_or("")
  }
}

/// One entry of the live-attempt map: the record, a receiver of its link watch, and the driver that runs it.
struct LiveEntry {
  core: Arc<Mutex<AttemptCore>>,
  link: watch::Receiver<LinkState>,
  driver: tokio::task::JoinHandle<()>,
}

/// The live attempt of each login name. A newer attempt is always the live one.
static LIVE: LazyLock<Mutex<HashMap<String, LiveEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Login names whose one-off link probe currently owns the login process slot.
static LINK_PROBES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// The guard that holds a login's process slot for one link probe. The probe is not a live attempt and never appears in
/// a row.
struct LinkProbeGuard(String);

impl Drop for LinkProbeGuard {
  fn drop(&mut self) {
    LINK_PROBES.lock().expect("the link-probe set is never poisoned").remove(&self.0);
  }
}

/// The login's attempt in progress, as a snapshot for the status table.
pub fn live_attempt(name: &str) -> Option<LiveAttempt> {
  let core = {
    let live = LIVE.lock().expect("the live-attempt map lock is never poisoned");
    let entry = live.get(name)?;
    Arc::clone(&entry.core)
  };
  let state = core.lock().expect("the attempt record lock is never poisoned");
  Some(LiveAttempt {
    phase: state.phase,
    approval_url: state.approval_url.clone(),
    started_at: state.started_at,
    input_open: state.input.is_some() && state.phase == Phase::WaitingForApproval,
  })
}

/// `start_login`: start an attempt of *login* and return its approval link. A click during a start joins that start,
/// while a click on a live attempt ends it with the outcome REPLACED. A live attempt holds its login's slot.
///
/// The attempt is registered before the first await, so a click that arrives while this start waits joins it rather
/// than starting a second process. Fails with the 502 body when the login gives no link or its state path is invalid.
///
/// A login with `use_pty` runs its command under a pseudo-terminal in a new session, so a keyboard-interactive prompt
/// reaches that terminal rather than the server's. Such a login may name an `enter_after_pattern`, which writes one
/// newline to the terminal on its first match. It may also name a `user_code_pattern`, whose first capture group is
/// appended to the approval link as `user_code=`.
pub async fn start_login(login: &BrowserLoginConfig) -> Result<String, StartFailed> {
  let state = host_auth::default_state_path()
    .map_err(|error| StartFailed(format!("could not resolve the host-auth state path: {error}")))?;
  let paths = AttemptPaths { state, ssh_config: host_auth::ssh_config_path() };
  start_login_with_paths(login, paths).await
}

/// `start_login_with_paths`: start an attempt, which records its outcome in the state file of `paths`.
async fn start_login_with_paths(login: &BrowserLoginConfig, paths: AttemptPaths) -> Result<String, StartFailed> {
  let mut link = match claim_slot(login, paths)? {
    Slot::Join(link) => link,
    Slot::Started { link, replaced } => {
      if let Some(core) = replaced {
        end(&core, Outcome::Replaced);
      }
      link
    }
  };
  await_link(&mut link).await
}

/// What a click gets from the live-attempt map: a start in progress to join, or its own new attempt and the one it
/// replaces.
enum Slot {
  Join(watch::Receiver<LinkState>),
  Started { link: watch::Receiver<LinkState>, replaced: Option<Arc<Mutex<AttemptCore>>> },
}

/// The click's slot: the login's start in progress to join, or a new attempt registered in its slot. The check and the
/// registration share one lock hold, so two clicks that arrive together cannot both start a process.
fn claim_slot(login: &BrowserLoginConfig, paths: AttemptPaths) -> Result<Slot, StartFailed> {
  let mut live = LIVE.lock().expect("the live-attempt map lock is never poisoned");
  if LINK_PROBES.lock().expect("the link-probe set is never poisoned").contains(&login.name) {
    return Err(StartFailed("a link probe is in progress for this login".to_string()));
  }
  if let Some(entry) = live.get(&login.name).filter(|entry| matches!(&*entry.link.borrow(), LinkState::Pending)) {
    return Ok(Slot::Join(entry.link.clone()));
  }
  let replaced = live.remove(&login.name).map(|entry| entry.core);
  let (link, link_rx) = watch::channel(LinkState::Pending);
  let core = Arc::new(Mutex::new(AttemptCore::new(login, link, paths)));
  let driver = tokio::spawn(drive(Arc::clone(&core)));
  live.insert(login.name.clone(), LiveEntry { core, link: link_rx.clone(), driver });
  Ok(Slot::Started { link: link_rx, replaced })
}

/// The link watch's answer to a waiting click: the link, or the 502 body of a start that gave none.
async fn await_link(link: &mut watch::Receiver<LinkState>) -> Result<String, StartFailed> {
  loop {
    match link.borrow_and_update().clone() {
      LinkState::Link(url) => return Ok(url),
      LinkState::Failed(body) => return Err(StartFailed(body)),
      LinkState::Pending => {}
    }
    if link.changed().await.is_err() {
      // The sender lives in the attempt record, which the live map holds until the attempt ends. A dropped sender means
      // the driver died without resolving the link.
      return Err(StartFailed(ABANDONED_START_BODY.to_string()));
    }
  }
}

/// `give_input`: write one value and its newline to the login that waits for it, then drop the sink. A login with an
/// `input_prompt` keeps that sink open while it waits for approval: its stdin pipe, or its terminal when `use_pty` is
/// set. The value is written once. A second value finds no sink and is refused.
pub async fn give_input(name: &str, value: String) -> Result<(), InputRefused> {
  let core = {
    let live = LIVE.lock().expect("the live-attempt map lock is never poisoned");
    Arc::clone(&live.get(name).ok_or(InputRefused::NotAwaiting)?.core)
  };
  let sink = {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    if state.phase != Phase::WaitingForApproval {
      return Err(InputRefused::NotAwaiting);
    }
    let sink = state.input.take().ok_or(InputRefused::NotAwaiting)?;
    state.input_submitted = true;
    sink
  };
  let mut bytes = value.into_bytes();
  bytes.push(b'\n');
  let written = match sink {
    InputSink::Pipe(mut stdin) => stdin.write_all(&bytes).await,
    InputSink::Terminal(master) => master.write_terminal(&bytes).await,
  };
  written.map_err(InputRefused::Write)
}

/// `check_readback`: run a readback once and answer how it ended. The login never runs here.
///
/// The capture runs as a task of its own, so its timeout ends the readback's process tree even when the request that
/// asked for the check has gone.
pub async fn check_readback(argv: &[String]) -> Readback {
  match start_readback(argv) {
    Ok(child) => finish_readback(child).await,
    Err(reason) => Readback::CannotRun { reason },
  }
}

/// `start_readback`: the readback's child, or the reason it cannot run.
fn start_readback(argv: &[String]) -> Result<Child, String> {
  if let Some(path) = missing_path(argv) {
    return Err(format!("missing file: {path}"));
  }
  spawn_readback(argv).map_err(|error| format!("could not start the readback command: {error}"))
}

/// `finish_readback`: the child's output and exit within the readback timeout. A supervisor's report that the command
/// could not start is a readback that cannot run.
async fn finish_readback(child: Child) -> Readback {
  let capture = tokio::spawn(finish_capture(child, Duration::from_secs(READBACK_TIMEOUT_SEC)));
  match capture.await.expect("the readback capture task does not panic") {
    Ok(Readback::Exited { output, .. }) if output.starts_with(ROUTE_ERROR_PREFIX) => {
      Readback::CannotRun { reason: output.strip_prefix(ROUTE_ERROR_PREFIX).unwrap_or_default().trim().to_string() }
    }
    Ok(readback) => readback,
    Err(error) => Readback::CannotRun { reason: format!("the readback did not finish: {error}") },
  }
}

/// `probe_link`: run the login's command until its approval link prints or the start timeout passes, then end its
/// process tree. The login is not approved here: no readback runs, and the output is dropped with the probe. The probe
/// is never registered as a live attempt, so its link reaches no row.
pub async fn probe_link(login: &BrowserLoginConfig) -> io::Result<LinkMatch> {
  probe_link_within(login, Duration::from_secs(START_TIMEOUT_SEC)).await
}

/// [`probe_link`] with its timeout as a parameter, so a test can run a silent login without waiting out the start
/// timeout.
async fn probe_link_within(login: &BrowserLoginConfig, timeout: Duration) -> io::Result<LinkMatch> {
  if let Some(path) = missing_path(&login.login_command) {
    tracing::warn!(login = %login.name, path, "browser_login_link_probe_missing_file");
    return Ok(LinkMatch::Fail);
  }
  let _probe = claim_link_probe(&login.name)?;
  let (mut child, mut output, _input) = match spawn_login(login) {
    Ok(spawned) => spawned,
    Err(error) => {
      tracing::warn!(login = %login.name, error = %error, "browser_login_link_probe_not_started");
      return Ok(LinkMatch::Fail);
    }
  };
  let matcher = Regex::new(&login.approval_url_pattern).expect("the config validated the approval-url pattern");
  let printed = tokio::time::timeout(timeout, link_printed(&matcher, &mut output)).await;
  // The login leads its own process group or session, so the tree ends with its root, descendants included.
  if let Some(pid) = child.id() {
    process_tree::kill_tree(pid)?;
  }
  child.wait().await?;
  match printed {
    Ok(Ok(true)) => Ok(LinkMatch::Pass),
    Ok(Ok(false)) => {
      tracing::warn!(login = %login.name, "browser_login_link_probe_no_link");
      Ok(LinkMatch::Fail)
    }
    Ok(Err(error)) => Err(error),
    Err(_elapsed) => {
      tracing::warn!(login = %login.name, "browser_login_link_probe_timed_out");
      Ok(LinkMatch::Fail)
    }
  }
}

/// The claim of a login's process slot for a link probe unless a full attempt already owns it.
fn claim_link_probe(name: &str) -> io::Result<LinkProbeGuard> {
  let live = LIVE.lock().expect("the live-attempt map lock is never poisoned");
  if live.contains_key(name) {
    return Err(io::Error::new(io::ErrorKind::WouldBlock, "a login attempt is in progress"));
  }
  let mut probes = LINK_PROBES.lock().expect("the link-probe set is never poisoned");
  if !probes.insert(name.to_string()) {
    return Err(io::Error::new(io::ErrorKind::WouldBlock, "a link probe is already in progress"));
  }
  Ok(LinkProbeGuard(name.to_string()))
}

/// Whether a line or partial line of the login's output matches the approval-link pattern. The output is read until it
/// matches or ends, without answering an enter prompt.
async fn link_printed(matcher: &Regex, output: &mut LoginReader) -> io::Result<bool> {
  loop {
    if output.seen_text().lines().any(|line| matcher.is_match(line)) {
      return Ok(true);
    }
    if !output.read_chunk().await? {
      return Ok(false);
    }
  }
}

/// `stop_all`: end every live attempt by killing its child — the driver's own paths then record each outcome — and
/// return once every driver finished.
///
/// The kill wakes every await a driver holds, so each driver reaches its recorded outcome. A driver whose spawn has
/// not yet been recorded keeps its own start timeout as a bound.
pub async fn stop_all() {
  let entries: Vec<LiveEntry> = {
    let mut live = LIVE.lock().expect("the live-attempt map lock is never poisoned");
    live.drain().map(|(_, entry)| entry).collect()
  };
  for entry in &entries {
    entry.core.lock().expect("the attempt record lock is never poisoned").kill_child();
  }
  for entry in entries {
    if let Err(error) = entry.driver.await {
      tracing::warn!(error = %error, "browser_login_driver_join_failed");
    }
  }
}

/// One attempt, from its start to its recorded outcome, then recorded in the state file. A driver whose run fails
/// ends the attempt too.
async fn drive(core: Arc<Mutex<AttemptCore>>) {
  if let Err(error) = run_attempt(&core).await {
    let login = core.lock().expect("the attempt record lock is never poisoned").login.name.clone();
    tracing::warn!(error = %error, login = %login, "browser_login_driver_failed");
    abandon(&core);
  }
  let (paths, name, bound, record) = {
    let state = core.lock().expect("the attempt record lock is never poisoned");
    (state.paths.clone(), state.login.name.clone(), state.login.host.is_some(), state.record())
  };
  let record = record.expect("a driver that returns has recorded its outcome");
  if let Err(error) = login_state::record_attempt(&paths.state, &name, bound, &record).await {
    tracing::error!(error = %format!("{error:#}"), login = %name, "browser_login_attempt_record_failed");
  }
  // The live slot is released after the record is written, so a status poll that sees no attempt in progress reads the
  // outcome as well.
  LIVE.lock().expect("the live-attempt map lock is never poisoned").retain(|_, entry| !Arc::ptr_eq(&entry.core, &core));
}

/// The attempt's run, from the spawned login to its outcome, with the output read for the approval link.
async fn run_attempt(core: &Arc<Mutex<AttemptCore>>) -> io::Result<()> {
  let login = core.lock().expect("the attempt record lock is never poisoned").login.clone();
  if let Some(path) = missing_path(&login.login_command) {
    fail_start(core, format!("missing file: {path}"));
    return Ok(());
  }
  let (mut child, mut output, input) = match spawn_login(&login) {
    Ok(spawned) => spawned,
    // An executable that is missing or not runnable.
    Err(error) => {
      fail_start(core, format!("could not start the login command: {error}"));
      return Ok(());
    }
  };
  {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    state.input = input;
  }
  record_child(core, child.id());
  tracing::info!(login = %login.name, "browser_login_started");

  let Some(link) = read_until_link(core, &mut output).await? else {
    child.start_kill().expect("a spawned child that has not been reaped takes the kill");
    let login_exit = proc_status::code_of(child.wait().await?).expect("an exit status has a code or a signal");
    let body = {
      let mut state = core.lock().expect("the attempt record lock is never poisoned");
      state.login_exit = Some(login_exit);
      state.child_pid = None;
      failure_text(login_exit, &state.output)
    };
    fail_start(core, body);
    return Ok(());
  };
  {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    state.approval_url = Some(link.clone());
    state.phase = Phase::WaitingForApproval;
    // A click already waiting on this watch is the click that started the attempt.
    let _ = state.link.send(LinkState::Link(link));
  }
  let name = core.lock().expect("the attempt record lock is never poisoned").login.name.clone();
  tracing::info!(login = %name, "browser_login_link_found");
  record_link_pass(core).await;
  hold(core, &mut child, &mut output).await
}

/// The link a started attempt printed, recorded as a pass: an earlier link-probe failure does not outlive a start that
/// worked. A failed record is logged. The attempt goes on.
async fn record_link_pass(core: &Arc<Mutex<AttemptCore>>) {
  let (paths, name) = {
    let state = core.lock().expect("the attempt record lock is never poisoned");
    (state.paths.clone(), state.login.name.clone())
  };
  if let Err(error) = login_state::record_link_match(&paths.state, &name, LinkMatch::Pass).await {
    tracing::error!(error = %format!("{error:#}"), login = %name, "browser_login_link_record_failed");
  }
}

/// The approval link from the first matching output line, with the login's user code appended when its
/// `user_code_pattern` matches. It is `None` when no link prints within the start timeout.
///
/// Once a link prints, the wait continues only for a user code the pattern has yet to capture, and the timeout then
/// answers the link it already has. The enter prompt is answered as the output arrives, in either wait.
async fn read_until_link(core: &Arc<Mutex<AttemptCore>>, output: &mut LoginReader) -> io::Result<Option<String>> {
  let deadline = tokio::time::Instant::now() + Duration::from_secs(START_TIMEOUT_SEC);
  let mut link: Option<String> = None;
  loop {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let next = match tokio::time::timeout(remaining, read_line(core, output)).await {
      Ok(next) => next?,
      Err(_elapsed) => break,
    };
    let Some(line) = next else { break };
    if link.is_none() {
      let matched = {
        let state = core.lock().expect("the attempt record lock is never poisoned");
        state.approval_url_matcher.find(&line).map(|matched| matched.as_str().to_string())
      };
      link = matched;
    }
    if link.is_some() {
      let user_code = user_code_in(core, &output.seen_text());
      let needs_user_code = core.lock().expect("the attempt record lock is never poisoned").user_code_matcher.is_some();
      if !needs_user_code || user_code.is_some() {
        return Ok(link.map(|link| with_user_code(link, user_code.as_deref())));
      }
    }
  }
  let user_code = user_code_in(core, &output.seen_text());
  Ok(link.map(|link| with_user_code(link, user_code.as_deref())))
}

/// The user code the login's `user_code_pattern` captures from `text`. It is `None` when the pattern is unset or
/// the text holds no match.
fn user_code_in(core: &Arc<Mutex<AttemptCore>>, text: &str) -> Option<String> {
  let state = core.lock().expect("the attempt record lock is never poisoned");
  let matcher = state.user_code_matcher.as_ref()?;
  matcher.captures(text)?.get(1).map(|group| group.as_str().to_string())
}

/// The approval link with the user code as the `user_code` query parameter that the login card reads.
fn with_user_code(link: String, user_code: Option<&str>) -> String {
  let Some(user_code) = user_code else { return link };
  let separator = if link.contains('?') { '&' } else { '?' };
  format!("{link}{separator}user_code={user_code}")
}

/// The login's output drained to its end and its exit awaited, within the hold limit. The check runs after that.
async fn hold(core: &Arc<Mutex<AttemptCore>>, child: &mut Child, output: &mut LoginReader) -> io::Result<()> {
  let login_exit =
    match tokio::time::timeout(Duration::from_secs(HOLD_LIMIT_SEC), drain_and_wait(core, child, output)).await {
      Ok(waited) => waited?,
      Err(_elapsed) => {
        child.start_kill().expect("a spawned child that has not been reaped takes the kill");
        let login_exit = proc_status::code_of(child.wait().await?).expect("an exit status has a code or a signal");
        {
          let mut state = core.lock().expect("the attempt record lock is never poisoned");
          state.login_exit = Some(login_exit);
          state.child_pid = None;
        }
        end(core, Outcome::HoldLimit);
        return Ok(());
      }
    };
  {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    state.login_exit = Some(login_exit);
    state.child_pid = None;
  }
  // A newer click ends a live attempt as REPLACED while its output drains. That attempt has nothing left to record.
  if core.lock().expect("the attempt record lock is never poisoned").outcome.is_some() {
    return Ok(());
  }
  if login_exit != 0 {
    end(core, Outcome::LoginFailed);
    return Ok(());
  }
  record_approval(core).await;
  read_back(core).await
}

/// The approval of a login that exited 0, recorded before its check runs, so the approval shows even when the check is
/// slow. A failed record is logged. The attempt goes on.
async fn record_approval(core: &Arc<Mutex<AttemptCore>>) {
  let (paths, name) = {
    let state = core.lock().expect("the attempt record lock is never poisoned");
    (state.paths.clone(), state.login.name.clone())
  };
  if let Err(error) = login_state::record_approval(&paths.state, &name, timestamps::utc_now()).await {
    tracing::error!(error = %format!("{error:#}"), login = %name, "browser_login_approval_record_failed");
  }
}

/// The check that follows an approval: the single-host probe for a host-bound login, else the login's readback. Its
/// result is the attempt's readback, and an approved login whose check passes is approved.
async fn read_back(core: &Arc<Mutex<AttemptCore>>) -> io::Result<()> {
  let (login, paths) = {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    state.phase = Phase::ReadingBack;
    (state.login.clone(), state.paths.clone())
  };
  let (readback_exit, readback_output) = match &login.host {
    Some(alias) => probe_bound_host(&paths, &login, alias).await,
    None => match start_readback(&login.readback_command) {
      Ok(child) => {
        record_child(core, child.id());
        match finish_readback(child).await {
          Readback::Exited { code, output } => (Some(code), output),
          Readback::TimedOut { after_sec } => (None, timeout_text(after_sec)),
          Readback::CannotRun { reason } => (None, reason),
        }
      }
      Err(reason) => (None, reason),
    },
  };
  {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    state.readback_exit = readback_exit;
    state.readback_output = readback_output;
    state.child_pid = None;
  }
  let outcome = if readback_exit == Some(0) { Outcome::Approved } else { Outcome::ReadbackFailed };
  end(core, outcome);
  Ok(())
}

/// The single-host probe of a host-bound login: its return code, and its detail as the output. A probe that cannot run
/// is a failed check, and the reason is its output.
async fn probe_bound_host(paths: &AttemptPaths, login: &BrowserLoginConfig, alias: &str) -> (Option<i32>, String) {
  match host_auth::probe_alias_with(&paths.state, &paths.ssh_config, alias, std::slice::from_ref(login), bound_probe)
    .await
  {
    Ok(answer) => (Some(answer.returncode), answer.login.detail),
    Err(error) => {
      tracing::error!(error = %format!("{error:#}"), login = %login.name, host = %alias, "browser_login_probe_failed");
      (None, format!("the probe did not run: {error:#}"))
    }
  }
}

/// The probe a host-bound login's check runs. Tests answer it from a fixture, so no ssh runs.
#[cfg(not(test))]
use crate::probe::probe_host_fresh as bound_probe;
#[cfg(test)]
use crate::test_support::bound_probe;

/// The output read to its end and the child's exit code.
async fn drain_and_wait(
  core: &Arc<Mutex<AttemptCore>>,
  child: &mut Child,
  output: &mut LoginReader,
) -> io::Result<i32> {
  while read_line(core, output).await?.is_some() {}
  Ok(proc_status::code_of(child.wait().await?).expect("an exit status has a code or a signal"))
}

/// The next output line, appended to the attempt's tail. It is `None` once the output has ended.
async fn read_line(core: &Arc<Mutex<AttemptCore>>, output: &mut LoginReader) -> io::Result<Option<String>> {
  let Some(line) = output.next_line().await? else { return Ok(None) };
  {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    state.output.push_back(line.clone());
    while state.output.len() > OUTPUT_KEEP_LINES {
      state.output.pop_front();
    }
  }
  Ok(Some(line))
}

/// A child running *argv* without a shell, in the server's environment, with stdout and stderr merged and the given stdin.
fn spawn(argv: &[String], stdin: Stdio) -> io::Result<Child> {
  let (program, arguments) = argv.split_first().expect("the config requires a non-empty command");
  let mut command = tokio::process::Command::new(program);
  command.args(arguments);
  spawn_command(command, stdin)
}

/// A readback under a dedicated subreaper so descendants stay discoverable after their parent exits.
fn spawn_readback(argv: &[String]) -> io::Result<Child> {
  #[cfg(test)]
  let command = {
    let (program, arguments) = argv.split_first().expect("the config requires a non-empty command");
    let mut command = tokio::process::Command::new(program);
    command.args(arguments);
    command
  };
  #[cfg(not(test))]
  let command = {
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command.arg(crate::process_tree::SUPERVISOR_ARG).args(argv);
    command
  };
  spawn_command(command, Stdio::null())
}

/// A spawned command in its own process group, with its output merged and the given stdin.
fn spawn_command(mut command: tokio::process::Command, stdin: Stdio) -> io::Result<Child> {
  // The child leads its own process group, so a readback's timeout can end every process it started.
  command.stdin(stdin).stdout(Stdio::piped()).process_group(0);
  // The child's own stderr writes into the same pipe as its stdout (`2>&1`).
  // SAFETY: `dup2` is async-signal-safe, and the closure runs between fork and exec with nothing else to touch.
  unsafe {
    command.pre_exec(|| {
      if libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) == -1 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    });
  }
  command.spawn()
}

/// The started login command, its output reader, and the input sink its `input_prompt` asks for. `use_pty` runs it under
/// a pseudo-terminal in a new session. Every other entry keeps stdout and stderr merged on a pipe, with stdin on a pipe
/// when it takes input and on /dev/null otherwise.
fn spawn_login(login: &BrowserLoginConfig) -> io::Result<(Child, LoginReader, Option<InputSink>)> {
  let enter_after = login.enter_after_pattern.as_deref();
  let takes_input = login.input_prompt.is_some();
  if login.use_pty {
    let (child, master) = spawn_pty(&login.login_command, !takes_input)?;
    let sink = takes_input.then(|| InputSink::Terminal(Arc::clone(&master)));
    Ok((child, LoginReader::new(OutputSource::Pty(master), enter_after), sink))
  } else {
    let stdin = if takes_input { Stdio::piped() } else { Stdio::null() };
    let mut child = spawn(&login.login_command, stdin)?;
    let stdout = child.stdout.take().expect("the login child pipes its merged output");
    let sink = takes_input.then(|| InputSink::Pipe(child.stdin.take().expect("the login child pipes its stdin")));
    Ok((child, LoginReader::new(OutputSource::Pipe(stdout), enter_after), sink))
  }
}

/// The login's output as whole lines, and the enter prompt the login waits on.
struct LoginReader {
  source: OutputSource,
  enter_after: Option<Regex>,
  /// The written newline is the only one this attempt sends, however often the pattern matches.
  entered: bool,
  /// Bytes read but not yet returned as a line.
  pending: Vec<u8>,
  /// The recent bytes, decoded for a pattern that can match a prompt with no trailing newline.
  seen: Vec<u8>,
}

/// The most output bytes a pattern is matched against. A prompt is short, so older output adds no match.
const SEEN_KEEP_BYTES: usize = 8192;

impl LoginReader {
  fn new(source: OutputSource, enter_after: Option<&str>) -> Self {
    LoginReader {
      source,
      enter_after: enter_after.map(|pattern| Regex::new(pattern).expect("the config validated the enter pattern")),
      entered: false,
      pending: Vec::new(),
      seen: Vec::new(),
    }
  }

  /// The next output line. The line in progress is returned as the last line when the output ends. Each chunk read
  /// also answers the enter prompt, which ssh prints without a trailing newline.
  async fn next_line(&mut self) -> io::Result<Option<String>> {
    loop {
      if let Some(position) = self.pending.iter().position(|byte| *byte == b'\n') {
        let line: Vec<u8> = self.pending.drain(..=position).collect();
        return Ok(Some(text_of(&line)));
      }
      if !self.read_chunk().await? {
        return Ok((!self.pending.is_empty()).then(|| text_of(&std::mem::take(&mut self.pending))));
      }
      self.answer_enter_prompt().await?;
    }
  }

  /// One output chunk, without answering the login's enter prompt.
  async fn read_chunk(&mut self) -> io::Result<bool> {
    let mut chunk = [0u8; 4096];
    let read = self.source.read(&mut chunk).await?;
    if read == 0 {
      return Ok(false);
    }
    self.pending.extend_from_slice(&chunk[..read]);
    self.seen.extend_from_slice(&chunk[..read]);
    if self.seen.len() > SEEN_KEEP_BYTES {
      let excess = self.seen.len() - SEEN_KEEP_BYTES;
      self.seen.drain(..excess);
    }
    Ok(true)
  }

  /// One newline written to the terminal when the output first matches the enter pattern.
  async fn answer_enter_prompt(&mut self) -> io::Result<()> {
    let Some(pattern) = &self.enter_after else { return Ok(()) };
    if self.entered || !pattern.is_match(&String::from_utf8_lossy(&self.seen)) {
      return Ok(());
    }
    self.entered = true;
    self.source.write_terminal(b"\n").await
  }

  /// The bytes seen so far, decoded, so a pattern can match a prompt that never ends its line.
  fn seen_text(&self) -> String {
    String::from_utf8_lossy(&self.seen).into_owned()
  }
}

/// The login child's output: a pipe for a login without `use_pty`, or the master side of its pseudo-terminal.
enum OutputSource {
  Pipe(ChildStdout),
  Pty(Arc<PtyMaster>),
}

impl OutputSource {
  /// *bytes* written to the login's terminal. A pipe carries no terminal, and validation makes
  /// `enter_after_pattern` require `use_pty`, so no entry asks a pipe to answer.
  async fn write_terminal(&self, bytes: &[u8]) -> io::Result<()> {
    match self {
      OutputSource::Pipe(_) => Ok(()),
      OutputSource::Pty(master) => master.write_terminal(bytes).await,
    }
  }
}

impl AsyncRead for OutputSource {
  fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
    match self.get_mut() {
      OutputSource::Pipe(stdout) => Pin::new(stdout).poll_read(cx, buf),
      OutputSource::Pty(master) => master.poll_read_terminal(cx, buf),
    }
  }
}

/// The master side of a pseudo-terminal, read for the login's output and written to answer its prompt.
struct PtyMaster {
  fd: AsyncFd<OwnedFd>,
}

impl PtyMaster {
  /// The master in its async wrapper, whose reads and writes the wrapper drives without blocking.
  fn new(fd: OwnedFd) -> io::Result<Arc<PtyMaster>> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
      return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
      return Err(io::Error::last_os_error());
    }
    // SAFETY: the owned descriptor stays open and unchanged inside the `AsyncFd` until the login ends.
    let fd = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE | Interest::WRITABLE) }
      .map_err(|error| error.into_parts().1)?;
    Ok(Arc::new(PtyMaster { fd }))
  }

  fn poll_read_terminal(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
    loop {
      let mut ready = match self.fd.poll_read_ready(cx) {
        Poll::Ready(Ok(ready)) => ready,
        Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
        Poll::Pending => return Poll::Pending,
      };
      let outcome = {
        let unfilled = buf.initialize_unfilled();
        ready.try_io(|inner| read_bytes(inner.get_ref().as_raw_fd(), unfilled))
      };
      match outcome {
        Ok(Ok(read)) => {
          buf.advance(read);
          return Poll::Ready(Ok(()));
        }
        // A master read answers EIO once no process holds the slave: the login's output has ended.
        Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => return Poll::Ready(Ok(())),
        Ok(Err(error)) => return Poll::Ready(Err(error)),
        Err(_would_block) => {}
      }
    }
  }

  async fn write_terminal(&self, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
      let mut ready = self.fd.writable().await?;
      let rest = &bytes[written..];
      match ready.try_io(|inner| write_bytes(inner.get_ref().as_raw_fd(), rest)) {
        Ok(Ok(count)) => written += count,
        Ok(Err(error)) => return Err(error),
        Err(_would_block) => {}
      }
    }
    Ok(())
  }
}

/// A child running *argv* under a pseudo-terminal of its own session, so ssh reads keyboard-interactive answers from
/// `/dev/tty` on that terminal instead of on the server's own. With `echo` off the terminal does not echo what is typed
/// into it, so a value typed for the login stays out of the login's output.
fn spawn_pty(argv: &[String], echo: bool) -> io::Result<(Child, Arc<PtyMaster>)> {
  let (program, arguments) = argv.split_first().expect("the config requires a non-empty command");
  let (master, slave) = open_pty()?;
  if !echo {
    disable_echo(slave.as_raw_fd())?;
  }
  let master = PtyMaster::new(master)?;
  let mut command = tokio::process::Command::new(program);
  command
    .args(arguments)
    .stdin(Stdio::from(slave.try_clone()?))
    .stdout(Stdio::from(slave.try_clone()?))
    .stderr(Stdio::from(slave));
  // SAFETY: `setsid` and `ioctl` are async-signal-safe, and the closure runs between fork and exec with nothing
  // else to touch. The child's standard descriptors are the terminal by the time this runs.
  unsafe {
    command.pre_exec(|| {
      if libc::setsid() == -1 {
        return Err(io::Error::last_os_error());
      }
      if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) == -1 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    });
  }
  let child = command.spawn()?;
  // The child holds its own copies of the slave. The parent closes its copies with this command, so a master read
  // ends at the child's exit.
  Ok((child, master))
}

/// A pseudo-terminal: the master side and the slave the login child owns.
///
/// Both carry close-on-exec. An unrelated fork of this process may hold the descriptors until it execs. A copy
/// of the slave that survived an exec would keep a master read from ending at the login's exit.
fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
  let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
  if master == -1 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: the descriptor is newly opened and no other owner holds it.
  let master = unsafe { OwnedFd::from_raw_fd(master) };
  if unsafe { libc::grantpt(master.as_raw_fd()) } == -1 || unsafe { libc::unlockpt(master.as_raw_fd()) } == -1 {
    return Err(io::Error::last_os_error());
  }
  let mut name: [libc::c_char; 128] = [0; 128];
  if unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) } != 0 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: `ptsname_r` wrote a NUL-terminated path into `name`.
  let slave = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
  if slave == -1 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: the descriptor is newly opened and no other owner holds it.
  Ok((master, unsafe { OwnedFd::from_raw_fd(slave) }))
}

/// The clearing of the terminal's echo of typed input.
fn disable_echo(fd: libc::c_int) -> io::Result<()> {
  // SAFETY: `termios` holds integers only, so the zeroed value is valid, and `tcgetattr` fills it for this descriptor.
  let mut term: libc::termios = unsafe { std::mem::zeroed() };
  if unsafe { libc::tcgetattr(fd, &mut term) } == -1 {
    return Err(io::Error::last_os_error());
  }
  term.c_lflag &= !libc::ECHO;
  // SAFETY: `term` is the attribute set `tcgetattr` just read, with the echo flag cleared.
  if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn read_bytes(fd: libc::c_int, buffer: &mut [u8]) -> io::Result<usize> {
  let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
  if read < 0 { Err(io::Error::last_os_error()) } else { Ok(read as usize) }
}

fn write_bytes(fd: libc::c_int, bytes: &[u8]) -> io::Result<usize> {
  let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
  if written < 0 { Err(io::Error::last_os_error()) } else { Ok(written as usize) }
}

/// One output line decoded, with its line ending removed.
fn text_of(bytes: &[u8]) -> String {
  String::from_utf8_lossy(bytes).trim_end_matches(['\r', '\n']).to_string()
}

/// A captured child's [`Readback`]: its exit code and merged output once its output ends and it exits. Past `limit`,
/// the child's whole process tree is killed, so no descendant keeps the output pipe open.
async fn finish_capture(mut child: Child, limit: Duration) -> io::Result<Readback> {
  let pid = child.id().expect("a child not yet waited for has a pid");
  let mut stdout = child.stdout.take().expect("the captured child pipes its merged output");
  let mut output = Vec::new();
  let capture = async {
    stdout.read_to_end(&mut output).await?;
    child.wait().await
  };
  match tokio::time::timeout(limit, capture).await {
    Ok(status) => {
      let code = proc_status::code_of(status?).expect("an exit status has a code or a signal");
      Ok(Readback::Exited { code, output: String::from_utf8_lossy(&output).into_owned() })
    }
    Err(_elapsed) => {
      process_tree::kill_tree(pid)?;
      child.wait().await?;
      Ok(Readback::TimedOut { after_sec: limit.as_secs() })
    }
  }
}

/// The 502 body: the exit code, then the last output lines.
fn failure_text(exit_code: i32, output: &VecDeque<String>) -> String {
  let mut lines = vec![format!("exit {exit_code}")];
  lines.extend(output.iter().rev().take(START_FAILURE_TAIL_LINES).rev().cloned());
  lines.join("\n")
}

/// A start failure recorded, and the clicks waiting on the link failed with *body*.
fn fail_start(core: &Arc<Mutex<AttemptCore>>, body: String) {
  let (name, login_exit) = {
    let state = core.lock().expect("the attempt record lock is never poisoned");
    (state.login.name.clone(), state.login_exit)
  };
  tracing::warn!(login = %name, login_exit = ?login_exit, "browser_login_start_failed");
  end(core, Outcome::StartFailed);
  // A click that joined the start awaits the watch. With no click waiting, nobody reads the failure.
  let _ = core.lock().expect("the attempt record lock is never poisoned").link.send(LinkState::Failed(body));
}

/// An attempt ended after its driver failed mid-run, so that no live slot or waiting click is left behind.
fn abandon(core: &Arc<Mutex<AttemptCore>>) {
  let phase = core.lock().expect("the attempt record lock is never poisoned").phase;
  match phase {
    Phase::Starting => fail_start(core, ABANDONED_START_BODY.to_string()),
    Phase::WaitingForApproval => end(core, Outcome::LoginFailed),
    Phase::ReadingBack => end(core, Outcome::ReadbackFailed),
  }
}

/// `end`: record *outcome* on the attempt and kill its live child, if one runs. The input sink closes with the attempt,
/// so a value that never came is never written. An attempt that already ended keeps its record: a newer click ends a
/// live attempt as REPLACED, and the replaced process's own exit then has nothing left to record.
fn end(core: &Arc<Mutex<AttemptCore>>, outcome: Outcome) {
  let name = {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    if state.outcome.is_some() {
      return;
    }
    state.kill_child();
    state.input = None;
    state.outcome = Some(outcome);
    state.finished_at = Some(timestamps::utc_now());
    state.login.name.clone()
  };
  let state = core.lock().expect("the attempt record lock is never poisoned");
  tracing::info!(
    login = %name,
    login_exit = ?state.login_exit,
    readback_exit = ?state.readback_exit,
    outcome = outcome.label(),
    "browser_login_finished"
  );
}

/// The live child's pid recorded, so a kill from outside the driver can reach it.
fn record_child(core: &Arc<Mutex<AttemptCore>>, pid: Option<u32>) {
  core.lock().expect("the attempt record lock is never poisoned").child_pid = pid;
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::Path;
  use std::time::Duration;

  use tempfile::TempDir;

  use serde_json::{Map, Value, json};

  use super::*;
  use crate::host_auth::State;
  use crate::login_state::LoginStatus;
  use crate::process_tree::is_running;

  const LINK: &str = "https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21";
  /// The stub body that prints the approval link, in the shape the AWS CLI prints it.
  const PRINT_LINK: &str = "printf '%s\\n' 'https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21'";
  const SSH_CONFIG: &str = "Host host1\n  HostName host1.example.internal\n";
  /// The tests share the module's records and the global stop, so they hold one lock for their whole run.
  static SERIAL: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(tokio::sync::Mutex::default);
  /// A login whose commands are stub scripts under a temporary home. Its state file and ssh config live there too.
  struct LoginStub {
    dir: TempDir,
    login: BrowserLoginConfig,
  }

  impl LoginStub {
    fn new(name: &str, login_body: &str, readback_body: &str) -> Self {
      let dir = tempfile::tempdir().expect("temporary directory exists");
      let login_script = write_script(dir.path(), &format!("{name}-login"), login_body);
      let readback_script = write_script(dir.path(), &format!("{name}-readback"), readback_body);
      let login = BrowserLoginConfig {
        name: name.to_string(),
        label: "AWS SSO".to_string(),
        host: None,
        login_command: vec![login_script],
        approval_url_pattern: r"https://device\.sso\S*".to_string(),
        readback_command: vec![readback_script],
        use_pty: false,
        enter_after_pattern: None,
        user_code_pattern: None,
        input_prompt: None,
        lifetime_sec: None,
      };
      LoginStub { dir, login }
    }

    /// The same stub, renewing the host `host1`.
    fn bound_to_host1(mut self) -> Self {
      self.login.host = Some("host1".to_string());
      self
    }

    fn paths(&self) -> AttemptPaths {
      let ssh_config = self.dir.path().join("ssh_config");
      fs::write(&ssh_config, SSH_CONFIG).expect("the ssh config writes");
      AttemptPaths { state: self.dir.path().join(host_auth::STATE_FILENAME), ssh_config }
    }
  }

  fn write_script(home: &Path, name: &str, body: &str) -> String {
    let path = home.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).expect("the stub script writes");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the stub becomes executable");
    path.to_str().expect("the temp path is UTF-8").to_string()
  }

  /// The wait until `check` holds, bounded so a driver that never reaches its state fails the test.
  async fn until(check: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !check() {
      assert!(tokio::time::Instant::now() < deadline, "the driver did not reach its state in time");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  }

  /// The stored record of `name`.
  fn login_record(paths: &AttemptPaths, name: &str) -> login_state::LoginState {
    let state = host_auth::read_state(&paths.state).expect("the state reads");
    login_state::login_state(&state, name).expect("the record reads")
  }

  /// The attempt record of `name` once an attempt with `outcome` has ended and recorded, with its slot released.
  async fn recorded_outcome(paths: &AttemptPaths, name: &str, outcome: Outcome) -> AttemptRecord {
    until(|| login_record(paths, name).last_attempt.is_some_and(|attempt| attempt.outcome == outcome)).await;
    until(|| live_attempt(name).is_none()).await;
    login_record(paths, name).last_attempt.expect("the attempt is recorded")
  }

  fn host_entry<'a>(state: &'a State, alias: &str) -> &'a Map<String, Value> {
    host_auth::host_entries(state)
      .expect("the hosts read")
      .into_iter()
      .find(|entry| entry.get("alias") == Some(&json!(alias)))
      .expect("the host is in the state")
  }

  #[test]
  fn user_code_reads_the_query_and_the_fragment_and_defaults_to_empty() {
    assert_eq!(user_code_of(LINK), "7f3c9d21");
    assert_eq!(user_code_of("https://view.awsapps.com/start#/device?user_code=abc123&x=1"), "abc123");
    assert_eq!(user_code_of("https://device.sso/no-code-here"), "");
  }

  #[test]
  fn the_user_code_becomes_the_query_parameter_the_login_card_reads() {
    let plain = "https://example.invalid/activate";
    assert_eq!(
      with_user_code(plain.to_string(), Some("ABCD-EFGH")),
      "https://example.invalid/activate?user_code=ABCD-EFGH"
    );
    assert_eq!(
      with_user_code("https://example.invalid/activate?x=1".to_string(), Some("ABCD-EFGH")),
      "https://example.invalid/activate?x=1&user_code=ABCD-EFGH"
    );
    assert_eq!(with_user_code(plain.to_string(), None), plain, "a login without a code keeps its link");
  }

  #[test]
  fn the_failure_body_carries_the_exit_code_and_the_last_lines() {
    let output = (0..30).map(|line| line.to_string()).collect::<VecDeque<_>>();
    let body = failure_text(3, &output);
    assert_eq!(body.lines().next(), Some("exit 3"));
    assert_eq!(body.lines().count(), START_FAILURE_TAIL_LINES + 1);
    assert_eq!(body.lines().last(), Some("29"));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn an_attempt_runs_the_login_the_link_and_the_readback_to_its_outcome() {
    let _serial = SERIAL.lock().await;
    let approved = LoginStub::new("approved", PRINT_LINK, "exit 0\n");
    let paths = approved.paths();
    let link = start_login_with_paths(&approved.login, paths.clone()).await.expect("the login gives its link");
    assert_eq!(link, LINK);
    let attempt = recorded_outcome(&paths, "approved", Outcome::Approved).await;
    assert_eq!(attempt.readback_exit, Some(0));
    assert_eq!(login_record(&paths, "approved").status, Some(LoginStatus::Ok));

    let failed = LoginStub::new("readback-failed", PRINT_LINK, "echo 'okta session expired'; exit 2\n");
    let paths = failed.paths();
    start_login_with_paths(&failed.login, paths.clone()).await.expect("the login gives its link");
    let attempt = recorded_outcome(&paths, "readback-failed", Outcome::ReadbackFailed).await;
    assert_eq!(attempt.readback_exit, Some(2));
    assert_eq!(attempt.readback_output.trim(), "okta session expired");
    assert_eq!(
      login_record(&paths, "readback-failed").status,
      Some(LoginStatus::Broken),
      "a check that fails after the login approved breaks the row"
    );
  }

  #[tokio::test]
  async fn the_approval_is_recorded_before_a_slow_check_finishes() {
    let _serial = SERIAL.lock().await;
    let slow = LoginStub::new("slow-check", PRINT_LINK, "while [ ! -e \"$0.release\" ]; do sleep 0.01; done\nexit 0\n");
    let paths = slow.paths();
    start_login_with_paths(&slow.login, paths.clone()).await.expect("the login gives its link");
    until(|| live_attempt("slow-check").is_some_and(|attempt| attempt.phase == Phase::ReadingBack)).await;
    until(|| login_record(&paths, "slow-check").last_approved_at.is_some()).await;
    assert!(
      login_record(&paths, "slow-check").last_attempt.is_none(),
      "the approval shows before the attempt's own record is written"
    );
    assert_eq!(login_record(&paths, "slow-check").link_match, Some(LinkMatch::Pass), "a started link passes its probe");
    fs::write(format!("{}.release", slow.login.readback_command[0]), "").expect("the readback is released");
    recorded_outcome(&paths, "slow-check", Outcome::Approved).await;
  }

  #[test]
  fn a_missing_program_or_script_is_named_and_a_later_data_path_is_not() {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    let program = write_script(dir.path(), "program", "exit 0\n");
    let script = write_script(dir.path(), "script", "exit 0\n");
    let data = dir.path().join("token-cache").display().to_string();
    let gone = dir.path().join("gone").display().to_string();
    let argv = |parts: &[&str]| parts.iter().map(|part| part.to_string()).collect::<Vec<String>>();
    assert_eq!(
      missing_path(&argv(&[&program, &script, &data])),
      None,
      "a data path that a login creates is not a script"
    );
    assert_eq!(missing_path(&argv(&[&program, &gone, &data])), Some(gone.as_str()), "a missing script is named");
    assert_eq!(missing_path(&argv(&[&gone, &script])), Some(gone.as_str()), "a missing program is named");
    assert_eq!(missing_path(&argv(&["ssh", "-o", "BatchMode=yes"])), None, "a bare program name is left to the spawn");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_link_probe_passes_on_its_link_and_ends_the_whole_process_tree() {
    let _serial = SERIAL.lock().await;
    let stub = LoginStub::new(
      "probe-linked",
      &format!("echo $$ > \"$0.root\"\nsleep 600 &\necho $! > \"$0.child\"\n{PRINT_LINK}\nsleep 600\n"),
      "touch \"$0.ran\"\nexit 0\n",
    );
    let matched = probe_link_within(&stub.login, Duration::from_secs(5)).await.expect("the probe runs");
    assert_eq!(matched, LinkMatch::Pass);
    let script = &stub.login.login_command[0];
    for pid_file in [format!("{script}.root"), format!("{script}.child")] {
      let pid: u32 =
        fs::read_to_string(&pid_file).expect("the login recorded its pid").trim().parse().expect("the pid parses");
      until(|| !is_running(pid)).await;
    }
    assert!(!stub.dir.path().join("probe-linked-readback.ran").exists(), "the probe ran no readback");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_link_probe_stops_before_answering_the_login_prompt() {
    let _serial = SERIAL.lock().await;
    let mut stub = LoginStub::new(
      "probe-no-enter",
      &format!(
        "printf '%s\\n' '{LINK} Press Enter after approval'\nif IFS= read -r answer; then touch \"$0.entered\"; fi\nsleep 600\n"
      ),
      "exit 0\n",
    );
    stub.login.use_pty = true;
    stub.login.enter_after_pattern = Some("Press Enter after approval".to_string());

    let matched = probe_link_within(&stub.login, Duration::from_secs(5)).await.expect("the probe runs");

    assert_eq!(matched, LinkMatch::Pass);
    assert!(!Path::new(&format!("{}.entered", stub.login.login_command[0])).exists(), "the probe sent no Enter");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_link_probe_reserves_its_login_against_a_concurrent_start() {
    let _serial = SERIAL.lock().await;
    let stub = LoginStub::new(
      "probe-reserves",
      &format!(
        "echo $$ >> \"$0.starts\"\nwhile [ ! -e \"$0.release\" ]; do sleep 0.001; done\n{PRINT_LINK}\nsleep 600\n"
      ),
      "exit 0\n",
    );
    let script = &stub.login.login_command[0];
    let starts = format!("{script}.starts");
    let login = stub.login.clone();
    let probe = tokio::spawn(async move { probe_link_within(&login, Duration::from_secs(5)).await });
    until(|| Path::new(&starts).exists()).await;

    let failure = start_login_with_paths(&stub.login, stub.paths()).await.expect_err("a probe owns the process slot");

    assert!(failure.0.contains("link probe is in progress"), "{failure:?}");
    assert_eq!(fs::read_to_string(&starts).expect("the start count reads").lines().count(), 1);
    fs::write(format!("{script}.release"), "").expect("the login is released");
    assert_eq!(probe.await.expect("the probe task completes").expect("the probe ends"), LinkMatch::Pass);
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_link_probe_that_prints_no_link_fails_at_its_timeout_and_ends_its_process() {
    let _serial = SERIAL.lock().await;
    let stub = LoginStub::new("probe-silent", "echo $$ > \"$0.root\"\nsleep 600\n", "exit 0\n");
    let matched = probe_link_within(&stub.login, Duration::from_millis(60)).await.expect("the probe runs");
    assert_eq!(matched, LinkMatch::Fail);
    let pid: u32 = fs::read_to_string(format!("{}.root", stub.login.login_command[0]))
      .expect("the login recorded its pid")
      .trim()
      .parse()
      .expect("the pid parses");
    until(|| !is_running(pid)).await;
  }

  #[tokio::test]
  async fn a_check_reports_its_merged_output_and_a_missing_script_cannot_run() {
    let _serial = SERIAL.lock().await;
    let stub = LoginStub::new("check", PRINT_LINK, "exit 0\n");
    let merged = write_script(stub.dir.path(), "merged", "echo valid; echo broken 1>&2\nexit 0\n");
    let Readback::Exited { code, output } = check_readback(std::slice::from_ref(&merged)).await else {
      panic!("the check ends within its limit");
    };
    assert_eq!(code, 0);
    assert_eq!(output.trim(), "valid\nbroken", "the child's own stderr reaches the same capture");

    let gone = stub.dir.path().join("gone.py").display().to_string();
    let Readback::CannotRun { reason } = check_readback(&[merged, gone.clone()]).await else {
      panic!("a check whose script is gone cannot run");
    };
    assert_eq!(reason, format!("missing file: {gone}"));
  }

  #[tokio::test]
  async fn a_host_bound_login_records_its_approval_and_its_probe_answers_the_host() {
    let _serial = SERIAL.lock().await;
    let _slot_lock = crate::test_support::round_slot_lock().lock().await;
    // The bound login's readback_command exits 9. Its check is the probe, so a readback that ran would fail the attempt.
    let approved = LoginStub::new("bound-approved", PRINT_LINK, "exit 9\n").bound_to_host1();
    let paths = approved.paths();
    crate::test_support::set_bound_answer(0, "");
    start_login_with_paths(&approved.login, paths.clone()).await.expect("the login gives its link");
    let attempt = recorded_outcome(&paths, "bound-approved", Outcome::Approved).await;
    assert_eq!(attempt.readback_exit, Some(0), "the probe's return code is the check's");
    let record = login_record(&paths, "bound-approved");
    assert!(record.last_approved_at.is_some(), "the login's exit 0 records the approval");
    assert_eq!(record.status, None, "a host-bound login's availability is its host's");
    let state = host_auth::read_state(&paths.state).expect("the state reads");
    assert_eq!(host_entry(&state, "host1")["status"], json!("ok"));

    let refused = LoginStub::new("bound-refused", PRINT_LINK, "exit 0\n").bound_to_host1();
    let paths = refused.paths();
    crate::test_support::set_bound_answer(255, "ssh: connect to host host1 port 22: Connection timed out");
    start_login_with_paths(&refused.login, paths.clone()).await.expect("the login gives its link");
    let attempt = recorded_outcome(&paths, "bound-refused", Outcome::ReadbackFailed).await;
    assert_eq!(attempt.readback_exit, Some(255));
    let state = host_auth::read_state(&paths.state).expect("the state reads");
    assert_eq!(host_entry(&state, "host1")["status"], json!("unreachable"));
    assert_eq!(
      login_record(&paths, "bound-refused").status,
      None,
      "a failed probe of a host-bound login is the host's"
    );
  }

  #[tokio::test]
  async fn a_start_without_a_link_and_a_missing_script_are_broken_and_a_refused_login_is_not() {
    let _serial = SERIAL.lock().await;
    let failing = LoginStub::new("start-failed", "echo starting; exit 3\n", "exit 0\n");
    let paths = failing.paths();
    let failure = start_login_with_paths(&failing.login, paths.clone()).await.expect_err("no link comes");
    assert_eq!(failure.0, "exit 3\nstarting");
    recorded_outcome(&paths, "start-failed", Outcome::StartFailed).await;
    let record = login_record(&paths, "start-failed");
    assert_eq!(record.status, Some(LoginStatus::Broken));
    assert_eq!(record.detail.as_deref(), Some("start failed: starting"));

    let mut missing = LoginStub::new("missing", PRINT_LINK, "exit 0\n");
    missing.login.login_command = vec![missing.dir.path().join("gone-login").display().to_string()];
    let paths = missing.paths();
    let failure = start_login_with_paths(&missing.login, paths.clone()).await.expect_err("the script is gone");
    assert!(failure.0.starts_with("missing file: "), "{}", failure.0);
    recorded_outcome(&paths, "missing", Outcome::StartFailed).await;
    assert_eq!(login_record(&paths, "missing").status, Some(LoginStatus::Broken));

    let refused = LoginStub::new("login-refused", &format!("{PRINT_LINK}\nexit 1\n"), "exit 0\n");
    let paths = refused.paths();
    start_login_with_paths(&refused.login, paths.clone()).await.expect("the login gives its link");
    recorded_outcome(&paths, "login-refused", Outcome::LoginFailed).await;
    assert_eq!(login_record(&paths, "login-refused").status, None, "a login that did not finish keeps its status");
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
  #[ignore = "integration"]
  async fn concurrent_clicks_on_one_login_start_one_process() {
    let _serial = SERIAL.lock().await;
    // The link prints after a delay, so every click arrives while the start is still pending. Each run appends a
    // line to a file named after its script, which counts the starts.
    let stub = LoginStub::new(
      "concurrent",
      &format!("echo started >> \"$0.starts\"\nsleep 1\n{PRINT_LINK}\nexec sleep 30\n"),
      "exit 0\n",
    );
    let paths = stub.paths();
    let clicks = (0..16).map(|_| {
      let login = stub.login.clone();
      let paths = paths.clone();
      tokio::spawn(async move { start_login_with_paths(&login, paths).await })
    });
    for click in futures_util::future::join_all(clicks).await {
      assert_eq!(click.expect("the click task finishes").expect("the click gets the link"), LINK);
    }
    let starts = fs::read_to_string(stub.dir.path().join("concurrent-login.starts")).expect("the login ran");
    assert_eq!(starts.lines().count(), 1, "one start serves every click");
    stop_all().await;
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_replaced_attempt_records_nothing_and_a_stop_records_the_live_one() {
    let _serial = SERIAL.lock().await;
    let stub = LoginStub::new("replaced", &format!("{PRINT_LINK}\nexec sleep 30\n"), "exit 0\n");
    let paths = stub.paths();
    let first = start_login_with_paths(&stub.login, paths.clone()).await.expect("the first click gets its link");
    until(|| live_attempt("replaced").is_some_and(|attempt| attempt.phase == Phase::WaitingForApproval)).await;
    let second = start_login_with_paths(&stub.login, paths.clone()).await.expect("the second click starts its attempt");
    assert_eq!(second, first, "the same stub prints the same link");

    // The server stop kills the live child, and the driver records the outcome a cancelled driver would.
    stop_all().await;
    recorded_outcome(&paths, "replaced", Outcome::LoginFailed).await;
    assert_eq!(live_attempt("replaced"), None);
  }

  #[tokio::test]
  async fn a_value_reaches_the_login_once_and_no_record_keeps_it() {
    let _serial = SERIAL.lock().await;
    let value = "CODE-7F3C-9D21";
    let body = format!(
      "{PRINT_LINK}\nread -r typed\nprintf '%s' \"$typed\" > \"$0.got\"\nprintf 'echoed=%s\\n' \"$typed\"\nexit 0\n"
    );
    let mut stub = LoginStub::new("with-input", &body, "echo valid\n");
    stub.login.input_prompt = Some("Paste the code".to_string());
    let paths = stub.paths();
    assert!(matches!(give_input("with-input", value.to_string()).await, Err(InputRefused::NotAwaiting)));
    start_login_with_paths(&stub.login, paths.clone()).await.expect("the login gives its link");
    until(|| live_attempt("with-input").is_some_and(|attempt| attempt.input_open)).await;
    give_input("with-input", value.to_string()).await.expect("the value is given");
    assert!(
      matches!(give_input("with-input", "again".to_string()).await, Err(InputRefused::NotAwaiting)),
      "the value is given once"
    );
    recorded_outcome(&paths, "with-input", Outcome::Approved).await;
    let got = fs::read_to_string(stub.dir.path().join("with-input-login.got")).expect("the login read its value");
    assert_eq!(got, value);
    let state = fs::read_to_string(&paths.state).expect("the state reads");
    assert!(!state.contains(value), "the value is in no state file");
    let attempt = login_record(&paths, "with-input").last_attempt.expect("the attempt is recorded");
    assert!(!attempt.last_line.contains(value), "the login's echo is not in the attempt record");
    assert!(!attempt.readback_output.contains(value), "the readback record has no login output");
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_pty_value_reaches_the_terminal_and_is_not_echoed_into_the_record() {
    let _serial = SERIAL.lock().await;
    let value = "CODE-PTY-4B2A";
    let body = format!("{PRINT_LINK}\nread -r typed < /dev/tty\nprintf '%s' \"$typed\" > \"$0.got\"\nexit 0\n");
    let mut stub = LoginStub::new("pty-input", &body, "echo valid\n");
    stub.login.use_pty = true;
    stub.login.input_prompt = Some("Paste the code".to_string());
    let paths = stub.paths();
    start_login_with_paths(&stub.login, paths.clone()).await.expect("the login gives its link");
    until(|| live_attempt("pty-input").is_some_and(|attempt| attempt.input_open)).await;
    give_input("pty-input", value.to_string()).await.expect("the value is given");
    recorded_outcome(&paths, "pty-input", Outcome::Approved).await;
    let got = fs::read_to_string(stub.dir.path().join("pty-input-login.got")).expect("the login read its value");
    assert_eq!(got, value);
    let state = fs::read_to_string(&paths.state).expect("the state reads");
    assert!(!state.contains(value), "the echo of the value is in no record");
  }
}
