//! Browser login: one click starts one login process, shows its approval link, and holds it to an outcome.
//!
//! Vocabulary:
//!
//! - A *login* is one `host_auth.browser_logins` entry.
//! - An *attempt* is one login process started by one click, from the click until its outcome is recorded. The
//!   readback phase is part of the attempt. A live attempt holds its login's slot; a click that starts a new attempt
//!   ends the live one with the outcome REPLACED.
//! - The *readback* is the login's check command. It exits 0 while the login is valid.
//!
//! Only [`start_login`] starts a login command. The page runs readback commands alone. Records live in memory, in
//! the two module maps: one live attempt per login name, and the last attempt that ended for it.
//!
//! A login with `use_pty` runs its command under a pseudo-terminal in a new session, so a keyboard-interactive
//! prompt reaches that terminal rather than the server's. Such a login may name an `enter_after_pattern`, which
//! writes one newline to the terminal on its first match, and a `user_code_pattern`, whose first capture group is
//! appended to the approval link as `user_code=`.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use regex::Regex;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, Interest, ReadBuf};
use tokio::process::{Child, ChildStdout};
use tokio::sync::watch;

use crate::config::BrowserLoginConfig;
use crate::host_auth;
use crate::timestamps;
use time::OffsetDateTime;

/// The approval link must print within this many seconds of the click. The AWS CLI prints it after at most two
/// requests, so a longer wait means the network or the config is broken.
const START_TIMEOUT_SEC: u64 = 30;
/// An AWS device code expires after 600 seconds. This limit only bounds a wedged child process.
const HOLD_LIMIT_SEC: u64 = 30 * 60;
const READBACK_TIMEOUT_SEC: u64 = 30;
const PAGE_READBACK_TIMEOUT_SEC: u64 = 20;
const OUTPUT_KEEP_LINES: usize = 200;
const START_FAILURE_TAIL_LINES: usize = 20;
/// The page shows at most this many characters of a readback run at page load.
pub const READBACK_SHOWN_CHARS: usize = 300;

/// The 502 body of a start whose driver died before the link or the failure was known.
const ABANDONED_START_BODY: &str = "the browser login ended before it gave an approval link";

/// How an attempt ended. An attempt still in progress holds no outcome yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
  Approved,
  LoginFailed,
  ReadbackFailed,
  Replaced,
  HoldLimit,
  StartFailed,
  RenewalRecordFailed,
}

impl Outcome {
  /// The page's word for this outcome.
  pub fn label(self) -> &'static str {
    match self {
      Outcome::Approved => "approved",
      Outcome::LoginFailed => "login failed",
      Outcome::ReadbackFailed => "readback failed",
      Outcome::Replaced => "replaced",
      Outcome::HoldLimit => "hold limit",
      Outcome::StartFailed => "start failure",
      Outcome::RenewalRecordFailed => "renewal record failed",
    }
  }
}

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

/// A start that produced no approval link. The string is the 502 body of the click that waited on it.
#[derive(Debug, Clone)]
pub struct StartFailed(pub String);

/// The state the link watch of one attempt carries.
#[derive(Debug, Clone)]
enum LinkState {
  /// The start is still producing the link.
  Pending,
  /// The link was found; every joined click redirects to it.
  Link(String),
  /// The start gave no link; the string is the 502 body of the clicks that waited on it.
  Failed(String),
}

/// `user_code_of`: the value of the link's first `user_code=` parameter, in the query or the fragment; empty when
/// none.
pub fn user_code_of(url: &str) -> &str {
  static USER_CODE_PARAM: OnceLock<Regex> = OnceLock::new();
  let pattern = USER_CODE_PARAM
    .get_or_init(|| Regex::new(r"[?&]user_code=([^&#]*)").expect("the built-in user-code pattern compiles"));
  pattern.captures(url).and_then(|captures| captures.get(1)).map(|matched| matched.as_str()).unwrap_or("")
}

/// The record one attempt keeps and the page reads. The attempt's driver task is the only writer, and every access
/// takes the mutex, so a page render never sees a half-written record.
struct AttemptCore {
  login: BrowserLoginConfig,
  started_at: OffsetDateTime,
  phase: Phase,
  approval_url: Option<String>,
  output: VecDeque<String>,
  login_exit: Option<i32>,
  readback_exit: Option<i32>,
  readback_output: String,
  outcome: Option<Outcome>,
  finished_at: Option<OffsetDateTime>,
  /// The live child's pid: the login process, then the readback process. A kill from outside the driver — a click
  /// that replaces the attempt, or the server stop — signals it.
  child_pid: Option<u32>,
  /// The link watch of this start: the approval link, or the 502 body. Every click of this start awaits it.
  link: watch::Sender<LinkState>,
  approval_url_matcher: Regex,
  user_code_matcher: Option<Regex>,
  renewal_state_path: Option<PathBuf>,
}

impl AttemptCore {
  fn new(login: &BrowserLoginConfig, link: watch::Sender<LinkState>, renewal_state_path: Option<PathBuf>) -> Self {
    AttemptCore {
      login: login.clone(),
      started_at: timestamps::utc_now(),
      phase: Phase::Starting,
      approval_url: None,
      output: VecDeque::new(),
      login_exit: None,
      readback_exit: None,
      readback_output: String::new(),
      outcome: None,
      finished_at: None,
      child_pid: None,
      link,
      approval_url_matcher: Regex::new(&login.approval_url_pattern)
        .expect("the config validated the approval-url pattern"),
      user_code_matcher: login
        .user_code_pattern
        .as_deref()
        .map(|pattern| Regex::new(pattern).expect("the config validated the user-code pattern")),
      renewal_state_path,
    }
  }

  /// Signal the recorded pid from outside the driver. An unsignalable pid is a child the driver's own wait already
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

  /// A read-only copy of the fields the page renders.
  fn snapshot(&self) -> AttemptSnapshot {
    AttemptSnapshot {
      phase: self.phase,
      approval_url: self.approval_url.clone(),
      started_at: self.started_at,
      finished_at: self.finished_at,
      outcome: self.outcome,
      last_line: self.output.iter().rev().find(|line| !line.trim().is_empty()).cloned().unwrap_or_default(),
      readback_exit: self.readback_exit,
      readback_output: self.readback_output.clone(),
    }
  }
}

/// A read-only copy of an attempt's record fields, taken under the record's mutex.
#[derive(Clone, Debug, PartialEq)]
pub struct AttemptSnapshot {
  pub phase: Phase,
  pub approval_url: Option<String>,
  pub started_at: OffsetDateTime,
  pub finished_at: Option<OffsetDateTime>,
  pub outcome: Option<Outcome>,
  /// The last output line that is not blank, or an empty string.
  pub last_line: String,
  pub readback_exit: Option<i32>,
  pub readback_output: String,
}

impl AttemptSnapshot {
  /// The device code the approval link carries; empty before the link is known.
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
/// The last attempt that ended, for each login name.
static LAST: LazyLock<Mutex<HashMap<String, Arc<Mutex<AttemptCore>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The login's attempt in progress, as a snapshot for the page.
pub fn live_attempt(name: &str) -> Option<AttemptSnapshot> {
  let core = {
    let live = LIVE.lock().expect("the live-attempt map lock is never poisoned");
    let entry = live.get(name)?;
    Arc::clone(&entry.core)
  };
  Some(core.lock().expect("the attempt record lock is never poisoned").snapshot())
}

/// The login's most recently ended attempt, as a snapshot for the page.
pub fn last_attempt(name: &str) -> Option<AttemptSnapshot> {
  let core = LAST.lock().expect("the last-attempt map lock is never poisoned").get(name)?.clone();
  Some(core.lock().expect("the attempt record lock is never poisoned").snapshot())
}

/// `start_login`: start an attempt of *login* and return its approval link; a click during a start joins that start.
///
/// The attempt is registered before the first await, so a click that arrives while this start waits joins it rather
/// than starting a second process. Fails with the 502 body when the login gives no link or its state path is invalid.
pub async fn start_login(login: &BrowserLoginConfig) -> Result<String, StartFailed> {
  let renewal_state_path = if login.host.is_some() {
    Some(
      host_auth::default_state_path()
        .map_err(|error| StartFailed(format!("could not resolve the host-auth state path: {error}")))?,
    )
  } else {
    None
  };
  start_login_with_state_path(login, renewal_state_path).await
}

/// `start_login_with_state_path`: start an attempt with the state file used by a confirmed host renewal.
async fn start_login_with_state_path(
  login: &BrowserLoginConfig,
  renewal_state_path: Option<PathBuf>,
) -> Result<String, StartFailed> {
  let mut link = match claim_slot(login, renewal_state_path) {
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
fn claim_slot(login: &BrowserLoginConfig, renewal_state_path: Option<PathBuf>) -> Slot {
  let mut live = LIVE.lock().expect("the live-attempt map lock is never poisoned");
  if let Some(entry) = live.get(&login.name).filter(|entry| matches!(&*entry.link.borrow(), LinkState::Pending)) {
    return Slot::Join(entry.link.clone());
  }
  let replaced = live.remove(&login.name).map(|entry| entry.core);
  let (link, link_rx) = watch::channel(LinkState::Pending);
  let core = Arc::new(Mutex::new(AttemptCore::new(login, link, renewal_state_path)));
  let driver = tokio::spawn(drive(Arc::clone(&core)));
  live.insert(login.name.clone(), LiveEntry { core, link: link_rx.clone(), driver });
  Slot::Started { link: link_rx, replaced }
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
      // The sender lives in the attempt record, which the last-attempt map holds forever; a dropped sender means
      // the driver died without resolving the link.
      return Err(StartFailed(ABANDONED_START_BODY.to_string()));
    }
  }
}

/// `page_readback`: run the login's readback once for the page: `(exit code, combined output)`. The login never runs
/// here.
pub async fn page_readback(login: &BrowserLoginConfig) -> io::Result<(i32, String)> {
  let child = spawn(&login.readback_command)?;
  finish_capture(child, Duration::from_secs(PAGE_READBACK_TIMEOUT_SEC)).await
}

/// `stop_all`: end every live attempt by killing its child — the driver's own paths then record each outcome — and
/// return once every driver finished.
///
/// The kill wakes every await a driver holds, so each driver reaches its recorded outcome; a driver whose spawn has
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

/// One attempt, from its start to its recorded outcome. A driver whose run fails ends the attempt too.
async fn drive(core: Arc<Mutex<AttemptCore>>) {
  if let Err(error) = run_attempt(&core).await {
    let login = core.lock().expect("the attempt record lock is never poisoned").login.name.clone();
    tracing::warn!(error = %error, login = %login, "browser_login_driver_failed");
    abandon(&core);
  }
}

/// The attempt's run, from the spawned login to its outcome, with the output read for the approval link.
async fn run_attempt(core: &Arc<Mutex<AttemptCore>>) -> io::Result<()> {
  let login = core.lock().expect("the attempt record lock is never poisoned").login.clone();
  let (mut child, mut output) = match spawn_login(&login) {
    Ok(spawned) => spawned,
    // An executable that is missing or not runnable.
    Err(error) => {
      fail_start(core, format!("could not start the login command: {error}"));
      return Ok(());
    }
  };
  record_child(core, child.id());
  tracing::info!(login = %login.name, "browser_login_started");

  let Some(link) = read_until_link(core, &mut output).await? else {
    child.start_kill().expect("a spawned child that has not been reaped takes the kill");
    let login_exit = exit_code(child.wait().await?);
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
  hold(core, &mut child, &mut output).await
}

/// The approval link from the first matching output line, with the login's user code appended when its
/// `user_code_pattern` matches; `None` when no link prints within the start timeout.
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

/// The user code the login's `user_code_pattern` captures from `text`; `None` when the pattern is unset or the
/// text holds no match.
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

/// The login's output drained to its end and its exit awaited, within the hold limit; then the readback.
async fn hold(core: &Arc<Mutex<AttemptCore>>, child: &mut Child, output: &mut LoginReader) -> io::Result<()> {
  let login_exit =
    match tokio::time::timeout(Duration::from_secs(HOLD_LIMIT_SEC), drain_and_wait(core, child, output)).await {
      Ok(waited) => waited?,
      Err(_elapsed) => {
        child.start_kill().expect("a spawned child that has not been reaped takes the kill");
        let login_exit = exit_code(child.wait().await?);
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
  // A newer click ends a live attempt as REPLACED while its output drains; that attempt has nothing left to record.
  if core.lock().expect("the attempt record lock is never poisoned").outcome.is_some() {
    return Ok(());
  }
  if login_exit != 0 {
    end(core, Outcome::LoginFailed);
    return Ok(());
  }
  read_back(core).await
}

/// The output read to its end and the child's exit code.
async fn drain_and_wait(
  core: &Arc<Mutex<AttemptCore>>,
  child: &mut Child,
  output: &mut LoginReader,
) -> io::Result<i32> {
  while read_line(core, output).await?.is_some() {}
  Ok(exit_code(child.wait().await?))
}

/// The readback: its command's run, and the confirmed renewal baseline recorded after a successful exit.
async fn read_back(core: &Arc<Mutex<AttemptCore>>) -> io::Result<()> {
  core.lock().expect("the attempt record lock is never poisoned").phase = Phase::ReadingBack;
  let readback_command = core.lock().expect("the attempt record lock is never poisoned").login.readback_command.clone();
  let child = match spawn(&readback_command) {
    Ok(child) => child,
    Err(error) => {
      core.lock().expect("the attempt record lock is never poisoned").readback_output =
        format!("could not start the readback command: {error}");
      end(core, Outcome::ReadbackFailed);
      return Ok(());
    }
  };
  record_child(core, child.id());
  let (readback_exit, readback_output) = finish_capture(child, Duration::from_secs(READBACK_TIMEOUT_SEC)).await?;
  {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    state.readback_exit = Some(readback_exit);
    state.readback_output = readback_output;
    state.child_pid = None;
  }
  let (already_ended, login_name, host, state_path) = {
    let state = core.lock().expect("the attempt record lock is never poisoned");
    (state.outcome.is_some(), state.login.name.clone(), state.login.host.clone(), state.renewal_state_path.clone())
  };
  if already_ended {
    return Ok(());
  }
  if readback_exit == 0
    && let Some(alias) = host
  {
    let state_path = state_path.expect("a host renewal has a resolved host-auth state path");
    if let Err(error) = host_auth::record_host_renewal(&state_path, &alias, timestamps::utc_now()).await {
      tracing::error!(error = %format!("{error:#}"), login = %login_name, host = %alias, "browser_login_renewal_record_failed");
      end(core, Outcome::RenewalRecordFailed);
      return Ok(());
    }
  }
  end(core, if readback_exit == 0 { Outcome::Approved } else { Outcome::ReadbackFailed });
  Ok(())
}

/// The next output line, appended to the attempt's tail; `None` once the output has ended.
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

/// A child running *argv* without a shell, in the server's environment, with stdin on /dev/null and output merged.
fn spawn(argv: &[String]) -> io::Result<Child> {
  let (program, arguments) = argv.split_first().expect("the config requires a non-empty command");
  let mut command = tokio::process::Command::new(program);
  command.args(arguments).stdin(Stdio::null()).stdout(Stdio::piped());
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

/// The started login command and its output reader. `use_pty` runs it under a pseudo-terminal in a new session; every
/// other entry keeps stdin on /dev/null with stdout and stderr merged on a pipe.
fn spawn_login(login: &BrowserLoginConfig) -> io::Result<(Child, LoginReader)> {
  let enter_after = login.enter_after_pattern.as_deref();
  if login.use_pty {
    let (child, master) = spawn_pty(&login.login_command)?;
    Ok((child, LoginReader::new(OutputSource::Pty(master), enter_after)))
  } else {
    let mut child = spawn(&login.login_command)?;
    let stdout = child.stdout.take().expect("the login child pipes its merged output");
    Ok((child, LoginReader::new(OutputSource::Pipe(stdout), enter_after)))
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

  /// The next output line; the line in progress is returned as the last line when the output ends. Each chunk read
  /// also answers the enter prompt, which ssh prints without a trailing newline.
  async fn next_line(&mut self) -> io::Result<Option<String>> {
    loop {
      if let Some(position) = self.pending.iter().position(|byte| *byte == b'\n') {
        let line: Vec<u8> = self.pending.drain(..=position).collect();
        return Ok(Some(text_of(&line)));
      }
      let mut chunk = [0u8; 4096];
      let read = self.source.read(&mut chunk).await?;
      if read == 0 {
        return Ok((!self.pending.is_empty()).then(|| text_of(&std::mem::take(&mut self.pending))));
      }
      self.pending.extend_from_slice(&chunk[..read]);
      self.seen.extend_from_slice(&chunk[..read]);
      if self.seen.len() > SEEN_KEEP_BYTES {
        let excess = self.seen.len() - SEEN_KEEP_BYTES;
        self.seen.drain(..excess);
      }
      self.answer_enter_prompt().await?;
    }
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
/// `/dev/tty` on that terminal instead of on the server's own.
fn spawn_pty(argv: &[String]) -> io::Result<(Child, Arc<PtyMaster>)> {
  let (program, arguments) = argv.split_first().expect("the config requires a non-empty command");
  let (master, slave) = open_pty()?;
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
  // The child holds its own copies of the slave; the parent's close with this command, so a master read ends at
  // the child's exit.
  Ok((child, master))
}

/// A pseudo-terminal: the master side and the slave the login child owns.
///
/// Both carry close-on-exec: an unrelated fork of this process may hold the descriptors until it execs, and a copy
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

/// A captured child's `(exit code, output)`. Past the limit it is killed, and its output
/// drained to the pipe's end.
async fn finish_capture(mut child: Child, limit: Duration) -> io::Result<(i32, String)> {
  let mut stdout = child.stdout.take().expect("the captured child pipes its merged output");
  let mut output = Vec::new();
  match tokio::time::timeout(limit, stdout.read_to_end(&mut output)).await {
    Ok(read) => {
      read?;
    }
    Err(_elapsed) => {
      child.start_kill().expect("a spawned child that has not been reaped takes the kill");
      stdout.read_to_end(&mut output).await?;
    }
  }
  let status = child.wait().await?;
  Ok((exit_code(status), String::from_utf8_lossy(&output).into_owned()))
}

/// `proc.returncode`: the exit code, or the negated signal number of a killed process.
fn exit_code(status: ExitStatus) -> i32 {
  status.code().or(status.signal().map(|signal| -signal)).expect("an exit status has a code or a signal")
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
  // A click that joined the start awaits the watch; with no click waiting, nobody reads the failure.
  let _ = core.lock().expect("the attempt record lock is never poisoned").link.send(LinkState::Failed(body));
}

/// End an attempt whose driver failed mid-run, so that no live slot or waiting click is left behind.
fn abandon(core: &Arc<Mutex<AttemptCore>>) {
  let phase = core.lock().expect("the attempt record lock is never poisoned").phase;
  match phase {
    Phase::Starting => fail_start(core, ABANDONED_START_BODY.to_string()),
    Phase::WaitingForApproval => end(core, Outcome::LoginFailed),
    Phase::ReadingBack => end(core, Outcome::ReadbackFailed),
  }
}

/// *outcome* recorded on the attempt, with the live child killed if one runs and the login's live slot released.
///
/// An attempt that already ended keeps its record: a newer click ends a live attempt as REPLACED, and the replaced
/// process's own exit then has nothing left to record.
fn end(core: &Arc<Mutex<AttemptCore>>, outcome: Outcome) {
  let (name, started_at) = {
    let mut state = core.lock().expect("the attempt record lock is never poisoned");
    if state.outcome.is_some() {
      return;
    }
    state.kill_child();
    state.outcome = Some(outcome);
    state.finished_at = Some(timestamps::utc_now());
    (state.login.name.clone(), state.started_at)
  };
  // A newer attempt is always the live one, so an older attempt's end clears neither the newer record nor the
  // newer slot in the last-attempt map.
  LIVE.lock().expect("the live-attempt map lock is never poisoned").retain(|_, entry| !Arc::ptr_eq(&entry.core, core));
  {
    let mut last = LAST.lock().expect("the last-attempt map lock is never poisoned");
    let newer_recorded = last.get(&name).is_some_and(|existing| {
      existing.lock().expect("the attempt record lock is never poisoned").started_at >= started_at
    });
    if !newer_recorded {
      last.insert(name.clone(), Arc::clone(core));
    }
  }
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

  use super::*;

  const LINK: &str = "https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21";
  /// The stub body that prints the approval link, in the shape the AWS CLI prints it.
  const PRINT_LINK: &str = "printf '%s\\n' 'https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21'";
  /// The tests share the module's records and the global stop, so they hold one lock for their whole run.
  static SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(tokio::sync::Mutex::default);

  /// A login whose commands are stub scripts under a temporary home. The config names commands as argv, so the
  /// stubs are executable shell scripts.
  struct LoginStub {
    _dir: TempDir,
    login: BrowserLoginConfig,
  }

  impl LoginStub {
    fn new(name: &str, login_body: &str, readback_body: &str) -> Self {
      let dir = tempfile::tempdir().expect("temporary directory exists");
      let login_script = write_script(dir.path(), &format!("{name}-login"), login_body);
      let readback_script = write_script(dir.path(), &format!("{name}-readback"), readback_body);
      LoginStub {
        _dir: dir,
        login: BrowserLoginConfig {
          name: name.to_string(),
          label: "AWS SSO".to_string(),
          host: None,
          login_command: vec![login_script],
          approval_url_pattern: r"https://device\.sso\S*".to_string(),
          readback_command: vec![readback_script],
          use_pty: false,
          enter_after_pattern: None,
          user_code_pattern: None,
        },
      }
    }
  }

  fn write_script(home: &Path, name: &str, body: &str) -> String {
    let path = home.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).expect("the stub script writes");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the stub becomes executable");
    path.to_str().expect("the temp path is UTF-8").to_string()
  }

  /// The wait until `check` holds, bounded so a driver that never reaches its outcome fails the test.
  async fn until(check: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !check() {
      assert!(tokio::time::Instant::now() < deadline, "the driver did not reach its outcome in time");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  }

  fn state_with_host_baseline(path: &Path, baseline: &str) {
    let mut state = host_auth::empty_state();
    state.insert("hosts".to_string(), serde_json::json!([{"alias": "host1", "enrolled_observed_at": baseline}]));
    host_auth::save_state(&state, path).expect("the host-auth state writes");
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
    let link = start_login(&approved.login).await.expect("the login gives its link");
    assert_eq!(link, LINK);
    until(|| matches!(last_attempt("approved"), Some(snapshot) if snapshot.outcome == Some(Outcome::Approved))).await;
    assert_eq!(last_attempt("approved").expect("the attempt ended").readback_exit, Some(0));
    assert_eq!(live_attempt("approved"), None, "the ended attempt released its slot");

    let failed = LoginStub::new("readback-failed", PRINT_LINK, "echo 'okta session expired'; exit 2\n");
    start_login(&failed.login).await.expect("the login gives its link");
    until(
      || matches!(last_attempt("readback-failed"), Some(snapshot) if snapshot.outcome == Some(Outcome::ReadbackFailed)),
    )
    .await;
    let last = last_attempt("readback-failed").expect("the attempt ended");
    assert_eq!(last.readback_exit, Some(2));
    assert_eq!(last.readback_output.trim(), "okta session expired");
  }

  #[tokio::test]
  async fn the_page_readback_reports_the_merged_output_without_starting_a_login() {
    let _serial = SERIAL.lock().await;
    let stub = LoginStub::new("page-readback", PRINT_LINK, "echo valid; echo broken 1>&2\nexit 0\n");
    let (exit, output) = page_readback(&stub.login).await.expect("the readback runs");
    assert_eq!(exit, 0);
    assert_eq!(output.trim(), "valid\nbroken", "the child's own stderr reaches the same capture");
  }

  #[tokio::test]
  async fn a_successful_host_renewal_moves_the_estimated_expiry() {
    let _serial = SERIAL.lock().await;
    let stub = LoginStub::new("renewal-recorded", PRINT_LINK, "echo valid\n");
    let mut login = stub.login.clone();
    login.host = Some("host1".to_string());
    let state_path = stub._dir.path().join("host_auth.json");
    let old_baseline = "2026-09-15T07:00:00+00:00";
    state_with_host_baseline(&state_path, old_baseline);

    assert_eq!(
      start_login_with_state_path(&login, Some(state_path.clone())).await.expect("the login gives its link"),
      LINK
    );
    until(|| matches!(last_attempt("renewal-recorded"), Some(snapshot) if snapshot.outcome == Some(Outcome::Approved)))
      .await;

    let attempt = last_attempt("renewal-recorded").expect("the attempt ended");
    let state = host_auth::read_state(&state_path).expect("the host-auth state reads");
    let entry = host_auth::host_entries(&state).expect("the host entries read")[0];
    let baseline = host_auth::opt_str(entry, "enrolled_observed_at").expect("the baseline field reads").unwrap();
    assert_ne!(baseline, old_baseline, "the successful renewal replaces the stale baseline");
    let renewed_at = timestamps::parse(baseline).expect("the recorded baseline parses");
    assert!(renewed_at >= attempt.started_at, "the baseline follows the renewal start");
    assert!(renewed_at <= attempt.finished_at.expect("the attempt has an end time"));
    let estimate = host_auth::derive_estimate(entry, renewed_at).expect("the renewed estimate derives");
    assert_eq!(estimate.remaining_sec, Some(host_auth::TRUST_CACHE_TTL_SEC as f64));
  }

  #[tokio::test]
  async fn a_failed_host_renewal_keeps_the_estimated_expiry() {
    let _serial = SERIAL.lock().await;
    let stub = LoginStub::new("renewal-failed", PRINT_LINK, "echo login expired; exit 1\n");
    let mut login = stub.login.clone();
    login.host = Some("host1".to_string());
    let state_path = stub._dir.path().join("host_auth.json");
    let old_baseline = "2026-09-15T07:00:00+00:00";
    state_with_host_baseline(&state_path, old_baseline);

    start_login_with_state_path(&login, Some(state_path.clone())).await.expect("the login gives its link");
    until(
      || matches!(last_attempt("renewal-failed"), Some(snapshot) if snapshot.outcome == Some(Outcome::ReadbackFailed)),
    )
    .await;

    let state = host_auth::read_state(&state_path).expect("the host-auth state reads");
    let entry = host_auth::host_entries(&state).expect("the host entries read")[0];
    assert_eq!(
      host_auth::opt_str(entry, "enrolled_observed_at").expect("the baseline field reads"),
      Some(old_baseline),
      "a failed readback leaves the baseline unchanged"
    );
    let old_expiry = host_auth::derive_estimate(entry, timestamps::utc_now()).expect("the old estimate derives");
    assert_eq!(
      old_expiry.expires_at.as_deref(),
      Some("2026-09-22T07:00:00+00:00"),
      "the failed renewal leaves the previous expiry estimate"
    );
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_failed_login_a_replaced_attempt_and_a_stop_end_their_attempts() {
    let _serial = SERIAL.lock().await;
    // A login command that gives no link: the click gets the exit code and the output tail.
    let failing = LoginStub::new("start-failed", "echo starting; exit 3\n", "exit 0\n");
    let failure = start_login(&failing.login).await.expect_err("no link comes");
    assert_eq!(failure.0, "exit 3\nstarting");
    until(|| matches!(last_attempt("start-failed"), Some(snapshot) if snapshot.outcome == Some(Outcome::StartFailed)))
      .await;

    // A click during a waiting attempt ends it as REPLACED and starts its own.
    let stub = LoginStub::new("replaced", &format!("{PRINT_LINK}\nsleep 30\n"), "exit 0\n");
    let first = start_login(&stub.login).await.expect("the first click gets its link");
    assert_eq!(live_attempt("replaced").expect("the first attempt is live").phase, Phase::WaitingForApproval);
    let second = start_login(&stub.login).await.expect("the second click starts its own attempt");
    assert_eq!(second, first, "the same stub prints the same link");
    let replaced = last_attempt("replaced").expect("the replaced attempt is recorded");
    assert_eq!(replaced.outcome, Some(Outcome::Replaced));

    // The server stop kills the live child, and the driver records the outcome a cancelled driver would.
    stop_all().await;
    let stopped = last_attempt("replaced").expect("the stopped attempt is recorded");
    assert_eq!(stopped.outcome, Some(Outcome::LoginFailed));
    assert_eq!(live_attempt("replaced"), None);
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
    let clicks = (0..16).map(|_| {
      let login = stub.login.clone();
      tokio::spawn(async move { start_login(&login).await })
    });
    for click in futures_util::future::join_all(clicks).await {
      assert_eq!(click.expect("the click task finishes").expect("the click gets the link"), LINK);
    }
    let starts = fs::read_to_string(stub._dir.path().join("concurrent-login.starts")).expect("the login ran");
    assert_eq!(starts.lines().count(), 1, "one start serves every click");
    stop_all().await;
  }
}
