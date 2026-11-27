//! The landing queue on origin: the turn that names the one writer of `main`, and the waiting landings that line up
//! behind it in the order their CI passed.
//!
//! Every write to `main` holds the turn. The queue lives only in git refs on origin, so no state is kept on this
//! host or in the server, and a host restart leaves nothing behind that a later client cannot read:
//!
//! - `refs/land-turn/main` names the holder of the turn;
//! - `refs/land-queue/<ci_passed_ms, 13 digits>-<pr>-<owner>` is one waiting landing.
//!
//! The owner is a session's `owner_token`, or a round's launch id as it is. No ref name and no heartbeat message
//! carries a session id. The pre-push hook refuses a session id in a commit message outside a View line, but it checks
//! a ref name only against the host patterns, so every queue-test origin installs the real scanner as a pre-receive
//! hook and checks each ref name and newly received commit message.
//!
//! Each ref points at a heartbeat commit: an empty commit whose parent is the commit to land, whose subject names the
//! holder, and whose committer time is the last heartbeat. A ref whose heartbeat is older than `DEAD_AFTER_SECS` is
//! dead, and any client that reads it deletes it with a lease and prints the name and the reason. Every change is a
//! push with a lease, and a change of several refs is one atomic push, so two clients never both create the turn and
//! never move one entry under each other.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use charliebot_store::layout;
use charliebot_store::metadata::read_metadata_value;
use charliebot_store::trigger_files;
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::git;
use crate::github_api::run_gh;

/// The turn ref. Git's receive-pack refuses a one-level name under `refs/`, so the turn sits one level deeper.
pub const TURN_REF: &str = "refs/land-turn/main";
/// The prefix of every waiting entry's ref.
pub const ENTRY_PREFIX: &str = "refs/land-queue/";
/// A heartbeat older than this many seconds marks its ref dead.
pub const DEAD_AFTER_SECS: i64 = 180;
/// The environment variable a command run under the turn carries: the turn holder's name, which the child's gate checks.
pub const TURN_ENV: &str = "CHARLIEBOT_LAND_TURN";
/// The trigger label prefix of every queued command. A landing's label is `land queue <sha12>`, a merge's is
/// `land queue pr <PR>`.
pub const WAKE_PREFIX: &str = "land queue ";
const PR_WAKE_PREFIX: &str = "land queue pr ";
/// The longest wait a queued command registers, in seconds: the trigger cap.
pub const WAKE_MAX_WAIT_SECS: i64 = 14400;
/// The bound of a round's foreground wait for the turn, in seconds. A round's tool call ends at 900 s
/// (`BASH_MAX_TIMEOUT_MS` in the round environment), which leaves room to merge and release after the wait; a test
/// asserts the margin.
pub const ROUND_WAIT_BOUND_SECS: i64 = 780;
/// A round's tool call limit, in seconds. The round's `BASH_MAX_TIMEOUT_MS` ends a foreground call here.
pub const ROUND_TOOL_CALL_SECS: i64 = 900;
/// Git's empty tree, the tree of every heartbeat commit.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const PUSH_TIMEOUT: Duration = Duration::from_secs(120);
const READ_ATTEMPTS: usize = 3;

/// How often a queued command polls origin and renews its heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
  pub poll: Duration,
  pub beat: Duration,
}

impl Cadence {
  pub const PRODUCTION: Cadence = Cadence { poll: Duration::from_secs(30), beat: Duration::from_secs(60) };

  /// The production cadence, with either interval shortened when a test override names it: the environment variables
  /// are the round-case tests' override, and no production caller sets them.
  pub fn from_env() -> Cadence {
    let overridden = |name: &str, default: Duration| {
      std::env::var(name).ok().and_then(|value| value.parse().ok()).map(Duration::from_millis).unwrap_or(default)
    };
    Cadence {
      poll: overridden("CHARLIEBOT_LAND_QUEUE_POLL_MS", Self::PRODUCTION.poll),
      beat: overridden("CHARLIEBOT_LAND_QUEUE_BEAT_MS", Self::PRODUCTION.beat),
    }
  }
}

/// The repository a queue lives in: the worktree whose `origin` holds the refs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
  pub worktree: PathBuf,
}

/// The session that owns a waiting entry and its wake: the entry name carries the session's `owner_token`, and the wake
/// is a trigger in the session's directory, registered and cancelled through `program` (`charliebot`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
  pub session_id: String,
  pub session_dir: PathBuf,
  pub worktree: PathBuf,
  pub program: PathBuf,
}

impl Owner {
  /// The owner key of this session's entries and turn: `owner_token` of its id.
  pub fn token(&self) -> anyhow::Result<String> {
    owner_token(&self.session_id)
  }
}

/// The owner token of a session: the first 16 hex digits of the SHA-256 of its id. The queue writes the token into its
/// ref names and heartbeat messages in place of the id, so the pushed text carries no session id. Sixteen digits, 64
/// bits, keep the entries of distinct sessions apart. An empty id names no session, so it is an error.
pub fn owner_token(session_id: &str) -> anyhow::Result<String> {
  anyhow::ensure!(!session_id.is_empty(), "the landing queue needs a session id to name its entry; nothing was queued");
  let digest = Sha256::digest(session_id.as_bytes());
  Ok(digest[..8].iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One waiting entry's name: its CI pass time in milliseconds, the PR, and its owner key, which is a session's token
/// or a round's launch id as it is. The derived order is the queue order: earliest CI pass first, then the lower PR
/// number, then the owner key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryName {
  pub ci_passed_ms: u64,
  pub pr: u64,
  pub owner: String,
}

impl EntryName {
  pub fn new(ci_passed_ms: u64, pr: u64, owner: &str) -> Self {
    Self { ci_passed_ms, pr, owner: owner.to_string() }
  }

  pub fn refname(&self) -> String {
    format!("{ENTRY_PREFIX}{:013}-{}-{}", self.ci_passed_ms, self.pr, self.owner)
  }

  /// The entry a ref name spells; a name that is not this form is an error, never skipped.
  pub fn parse(refname: &str) -> anyhow::Result<Self> {
    let rest =
      refname.strip_prefix(ENTRY_PREFIX).ok_or_else(|| anyhow::anyhow!("{refname} is not a landing queue entry"))?;
    let (ms, rest) = rest.split_once('-').ok_or_else(|| anyhow::anyhow!("{refname} has no CI pass time"))?;
    let (pr, owner) = rest.split_once('-').ok_or_else(|| anyhow::anyhow!("{refname} has no PR number"))?;
    if ms.len() != 13 || owner.is_empty() {
      anyhow::bail!("{refname} is not <13-digit ms>-<pr>-<owner>");
    }
    Ok(Self {
      ci_passed_ms: ms.parse().with_context(|| format!("{refname} has a bad CI pass time"))?,
      pr: pr.parse().with_context(|| format!("{refname} has a bad PR number"))?,
      owner: owner.to_string(),
    })
  }
}

/// One live ref of the queue with the heartbeat it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Beat {
  pub refname: String,
  pub sha: String,
  pub parent: String,
  pub beat_at: i64,
  pub holder: String,
}

impl Beat {
  fn age(&self, now: i64) -> i64 {
    now - self.beat_at
  }
}

/// The queue as one read saw it: the turn, when held, and the live entries in queue order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueView {
  pub turn: Option<Beat>,
  pub entries: Vec<(EntryName, Beat)>,
}

impl QueueView {
  /// The live entry of `pr` that `owner` (an owner key) owns, when it waits.
  pub fn entry_of(&self, pr: u64, owner: &str) -> Option<&(EntryName, Beat)> {
    self.entries.iter().find(|(name, _)| name.pr == pr && name.owner == owner)
  }
}

/// Whether the turn that `holder` names belongs to `owner` (an owner key) on `pr`. The holder of an entry's turn is
/// that entry's ref name; a direct writer's holder (`direct pr <PR>`) is no entry, so it belongs to no owner.
pub fn turn_belongs_to(holder: &str, pr: u64, owner: &str) -> bool {
  EntryName::parse(holder).is_ok_and(|name| name.pr == pr && name.owner == owner)
}

/// Whether a client may take the turn now. The turn must be free; with waiting entries, only the earliest may take it,
/// and a writer with no entry (`mine` is `None`) waits behind any entry.
pub fn may_take(view: &QueueView, mine: Option<&str>) -> bool {
  if view.turn.is_some() {
    return false;
  }
  match (view.entries.first(), mine) {
    (None, _) => true,
    (Some((first, _)), Some(refname)) => first.refname() == refname,
    (Some(_), None) => false,
  }
}

/// One ref change of a push. A lease names the value the ref must still hold: `Create` requires it absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
  Create { refname: String, new: String },
  Move { refname: String, from: String, new: String },
  Delete { refname: String, from: String },
}

impl Change {
  pub fn refname(&self) -> &str {
    match self {
      Self::Create { refname, .. } | Self::Move { refname, .. } | Self::Delete { refname, .. } => refname,
    }
  }
}

/// The outcome of one push of changes: all applied, or refused with git's stderr.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pushed {
  Done,
  Refused(String),
}

/// What `Claim::take_turn` found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
  Taken,
  /// Another holder has the turn; the claim keeps waiting.
  Busy,
  /// The claim's entry moved or vanished while it tried.
  Lost,
}

/// What one turn step of a waiting claim found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
  Held,
  Waiting,
}

impl Site {
  /// Reads the queue from origin. A dead ref is deleted first, with a message naming it and the reason.
  pub async fn read(&self) -> anyhow::Result<QueueView> {
    let now = now_secs();
    let mut turn = None;
    let mut entries = Vec::new();
    for beat in self.beats().await? {
      if beat.age(now) > DEAD_AFTER_SECS {
        self.prune(&beat, now).await?;
        continue;
      }
      if beat.refname == TURN_REF {
        turn = Some(beat);
      } else {
        entries.push((EntryName::parse(&beat.refname)?, beat));
      }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(QueueView { turn, entries })
  }

  /// Applies `changes` in one atomic push with a lease on each ref they touch.
  pub async fn push(&self, changes: &[Change]) -> anyhow::Result<Pushed> {
    let mut args = vec!["push".to_string(), "--atomic".to_string()];
    let mut refspecs = Vec::with_capacity(changes.len());
    for change in changes {
      match change {
        Change::Create { refname, new } => {
          args.push(format!("--force-with-lease={refname}:"));
          refspecs.push(format!("{new}:{refname}"));
        }
        Change::Move { refname, from, new } => {
          args.push(format!("--force-with-lease={refname}:{from}"));
          refspecs.push(format!("{new}:{refname}"));
        }
        Change::Delete { refname, from } => {
          args.push(format!("--force-with-lease={refname}:{from}"));
          refspecs.push(format!(":{refname}"));
        }
      }
    }
    args.push("origin".to_string());
    args.extend(refspecs);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (ok, _, err) = git::git_stdout(&self.worktree, &argv, PUSH_TIMEOUT, "git push").await;
    Ok(if ok { Pushed::Done } else { Pushed::Refused(err) })
  }

  /// Every queue ref on origin, read with its heartbeat. The refs are fetched as objects with no local ref kept.
  async fn beats(&self) -> anyhow::Result<Vec<Beat>> {
    let mut last_error = String::new();
    for _ in 0..READ_ATTEMPTS {
      let listed =
        git_ok(&self.worktree, &["ls-remote", "origin", TURN_REF, "refs/land-queue/*"], GIT_TIMEOUT, "git ls-remote")
          .await?;
      let refs: Vec<(String, String)> = listed
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(sha, name)| (sha.to_string(), name.to_string()))
        .collect();
      if refs.is_empty() {
        return Ok(Vec::new());
      }
      let mut fetch = vec!["fetch", "--no-write-fetch-head", "--no-tags", "--quiet", "origin"];
      fetch.extend(refs.iter().map(|(_, name)| name.as_str()));
      let (ok, _, err) = git::git_stdout(&self.worktree, &fetch, GIT_TIMEOUT, "git fetch queue").await;
      if !ok {
        // A ref listed a moment ago may have been deleted since; the listing is read again.
        last_error = err;
        continue;
      }
      let mut beats = Vec::with_capacity(refs.len());
      let mut changed = false;
      for (sha, refname) in refs {
        let Some(beat) = self.beat_of(&sha, &refname).await? else {
          last_error = format!("{refname} moved after it was listed at {sha}");
          changed = true;
          break;
        };
        beats.push(beat);
      }
      if changed {
        continue;
      }
      return Ok(beats);
    }
    anyhow::bail!("the landing queue kept changing while it was read in {}: {last_error}", self.worktree.display())
  }

  async fn beat_of(&self, sha: &str, refname: &str) -> anyhow::Result<Option<Beat>> {
    let (ok, shown, err) =
      git::git_stdout(&self.worktree, &["show", "-s", "--format=%ct%x09%P%x09%s", sha], GIT_TIMEOUT, "git show").await;
    if !ok {
      if err == format!("fatal: bad object {sha}") {
        return Ok(None);
      }
      anyhow::bail!("git show failed in {}: {err}", self.worktree.display());
    }
    let fields: Vec<&str> = shown.splitn(3, '\t').collect();
    let (beat_at, parents, holder) = match fields.as_slice() {
      [beat_at, parents, holder] => (*beat_at, *parents, *holder),
      _ => anyhow::bail!("{refname} points at {sha}, which is not a heartbeat commit"),
    };
    let parent = parents
      .split_whitespace()
      .next()
      .ok_or_else(|| anyhow::anyhow!("{refname} points at {sha}, a heartbeat without a parent"))?;
    Ok(Some(Beat {
      refname: refname.to_string(),
      sha: sha.to_string(),
      parent: parent.to_string(),
      beat_at: beat_at.parse().with_context(|| format!("{sha} has no commit time"))?,
      holder: holder.to_string(),
    }))
  }

  async fn prune(&self, beat: &Beat, now: i64) -> anyhow::Result<()> {
    let change = Change::Delete { refname: beat.refname.clone(), from: beat.sha.clone() };
    match self.push(&[change]).await? {
      Pushed::Done => {
        eprintln!("pruned {}: its heartbeat is {}s old, past the {DEAD_AFTER_SECS}s limit", beat.refname, beat.age(now))
      }
      Pushed::Refused(err) => eprintln!("kept {}: it moved while it was read: {err}", beat.refname),
    }
    Ok(())
  }

  /// Makes sure `sha` is a local commit object, fetching it from origin when the worktree does not hold it. The
  /// heartbeat commit names the commit to land as its parent, so that commit must be present before the beat.
  async fn ensure_commit(&self, sha: &str) -> anyhow::Result<()> {
    if git::git_rev_parse(&self.worktree, sha).await.as_deref() == Some(sha) {
      return Ok(());
    }
    let (ok, _, err) =
      git::git_stdout(&self.worktree, &["fetch", "--no-tags", "--quiet", "origin", sha], GIT_TIMEOUT, "git fetch")
        .await;
    if !ok {
      anyhow::bail!("git fetch origin {sha} failed in {}: {err}", self.worktree.display());
    }
    Ok(())
  }

  /// Writes one heartbeat commit on `parent`, named `holder`, and returns its sha.
  async fn commit_beat(&self, parent: &str, holder: &str) -> anyhow::Result<String> {
    self.ensure_commit(parent).await?;
    let stamp = format!("@{} +0000", now_secs());
    let mut command = tokio::process::Command::new("git");
    command
      .arg("-C")
      .arg(&self.worktree)
      .args(["-c", "commit.gpgsign=false", "commit-tree", "--no-gpg-sign", EMPTY_TREE, "-p", parent, "-m", holder])
      .env("GIT_AUTHOR_NAME", "charliebot land queue")
      .env("GIT_AUTHOR_EMAIL", "land-queue@charliebot.invalid")
      .env("GIT_COMMITTER_NAME", "charliebot land queue")
      .env("GIT_COMMITTER_EMAIL", "land-queue@charliebot.invalid")
      .env("GIT_AUTHOR_DATE", &stamp)
      .env("GIT_COMMITTER_DATE", &stamp)
      .kill_on_drop(true);
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
      .await
      .map_err(|_| anyhow::anyhow!("git commit-tree timed out in {}", self.worktree.display()))?
      .with_context(|| format!("git commit-tree failed to start in {}", self.worktree.display()))?;
    if !output.status.success() {
      anyhow::bail!(
        "git commit-tree failed in {}: {}",
        self.worktree.display(),
        String::from_utf8_lossy(&output.stderr).trim()
      );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
  }
}

/// One client's place in the queue: the entry it waits in (absent for a direct writer of `main`) and the turn, once
/// it holds it. Every change it makes is a lease on the value it wrote last.
#[derive(Debug)]
pub struct Claim {
  site: Site,
  /// The name that the turn and the entry carry: the entry's ref name, or `direct pr <PR>` for a direct writer.
  pub holder: String,
  /// The commit to land, the parent of every heartbeat this claim writes.
  pub parent: String,
  entry: Option<Ref>,
  turn: Option<String>,
  beat_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ref {
  refname: String,
  sha: String,
}

impl Claim {
  /// Enters the queue as `name` for the commit `parent`, which must be on origin.
  pub async fn enter(site: &Site, name: &EntryName, parent: &str) -> anyhow::Result<Claim> {
    let holder = name.refname();
    let sha = site.commit_beat(parent, &holder).await?;
    let change = Change::Create { refname: holder.clone(), new: sha.clone() };
    match site.push(&[change]).await? {
      Pushed::Done => Ok(Claim {
        site: site.clone(),
        holder: holder.clone(),
        parent: parent.to_string(),
        entry: Some(Ref { refname: holder, sha }),
        turn: None,
        beat_at: Instant::now(),
      }),
      Pushed::Refused(err) => anyhow::bail!("could not enter the landing queue as {holder}: {err}"),
    }
  }

  /// The claim of an entry a read found, with the turn when the queue shows that entry holding it.
  fn from_beat(site: &Site, view: &QueueView, refname: &str, beat: &Beat) -> Claim {
    let turn = view.turn.as_ref().filter(|turn| turn.holder == refname).map(|turn| turn.sha.clone());
    Claim {
      site: site.clone(),
      holder: refname.to_string(),
      parent: beat.parent.clone(),
      entry: Some(Ref { refname: refname.to_string(), sha: beat.sha.clone() }),
      turn,
      beat_at: Instant::now(),
    }
  }

  /// Enters the queue as `name` for `parent`, or resumes the entry this owner already waits in: an entry for the same
  /// PR and owner keeps its place, and one that points at an older commit moves to `parent`.
  pub async fn enter_or_resume(site: &Site, name: &EntryName, parent: &str) -> anyhow::Result<Claim> {
    let view = site.read().await?;
    let Some((_, beat)) = view.entry_of(name.pr, &name.owner) else {
      return Claim::enter(site, name, parent).await;
    };
    if beat.parent == parent {
      return Ok(Claim::from_beat(site, &view, &name.refname(), beat));
    }
    let sha = site.commit_beat(parent, &name.refname()).await?;
    let change = Change::Move { refname: name.refname(), from: beat.sha.clone(), new: sha.clone() };
    match site.push(&[change]).await? {
      Pushed::Done => Ok(Claim {
        site: site.clone(),
        holder: name.refname(),
        parent: parent.to_string(),
        entry: Some(Ref { refname: name.refname(), sha }),
        turn: None,
        beat_at: Instant::now(),
      }),
      Pushed::Refused(err) => anyhow::bail!("could not resume the landing queue entry of {}: {err}", name.refname()),
    }
  }

  /// Whether this claim holds the turn.
  pub fn holds_turn(&self) -> bool {
    self.turn.is_some()
  }

  /// A claim with no entry: a direct writer of `main` for PR `pr`, which holds the turn only.
  pub fn direct(site: &Site, pr: u64, parent: &str) -> Claim {
    Claim {
      site: site.clone(),
      holder: format!("direct pr {pr}"),
      parent: parent.to_string(),
      entry: None,
      turn: None,
      beat_at: Instant::now(),
    }
  }

  pub fn entry_refname(&self) -> Option<&str> {
    self.entry.as_ref().map(|entry| entry.refname.as_str())
  }

  /// The changes of the write that lands `new` on `main_ref` when `from` is the tip the head was tested on: main
  /// moves under its lease, and the turn and this claim's entry are deleted in the same atomic push.
  pub fn publish_changes(&self, main_ref: &str, from: &str, new: &str) -> Vec<Change> {
    let mut changes =
      vec![Change::Move { refname: main_ref.to_string(), from: from.to_string(), new: new.to_string() }];
    changes.extend(self.held_deletes());
    changes
  }

  /// Forgets the refs a publish removed in its atomic push, so that a later `release` has nothing to delete.
  pub fn landed(&mut self) {
    self.turn = None;
    self.entry = None;
  }

  /// Creates the turn and, when the claim waits, moves its entry to the turn's heartbeat in the same push.
  pub async fn take_turn(&mut self) -> anyhow::Result<Take> {
    let sha = self.site.commit_beat(&self.parent, &self.holder).await?;
    let mut changes = vec![Change::Create { refname: TURN_REF.to_string(), new: sha.clone() }];
    if let Some(entry) = &self.entry {
      changes.push(Change::Move { refname: entry.refname.clone(), from: entry.sha.clone(), new: sha.clone() });
    }
    match self.site.push(&changes).await? {
      Pushed::Done => {
        self.turn = Some(sha.clone());
        if let Some(entry) = &mut self.entry {
          entry.sha = sha;
        }
        self.beat_at = Instant::now();
        Ok(Take::Taken)
      }
      Pushed::Refused(_) => {
        let view = self.site.read().await?;
        Ok(if view.turn.is_some() { Take::Busy } else { Take::Lost })
      }
    }
  }

  /// Whether this claim's heartbeat is due.
  pub fn beat_due(&self, cadence: Cadence) -> bool {
    self.beat_at.elapsed() >= cadence.beat
  }

  /// Moves the held refs to a new heartbeat on `parent`. `Pushed::Refused` means a ref is no longer this claim's.
  pub async fn beat(&mut self, parent: &str) -> anyhow::Result<Pushed> {
    let sha = self.site.commit_beat(parent, &self.holder).await?;
    let mut changes = Vec::new();
    if let Some(turn) = &self.turn {
      changes.push(Change::Move { refname: TURN_REF.to_string(), from: turn.clone(), new: sha.clone() });
    }
    if let Some(entry) = &self.entry {
      changes.push(Change::Move { refname: entry.refname.clone(), from: entry.sha.clone(), new: sha.clone() });
    }
    if changes.is_empty() {
      return Ok(Pushed::Done);
    }
    let pushed = self.site.push(&changes).await?;
    if pushed == Pushed::Done {
      self.parent = parent.to_string();
      if self.turn.is_some() {
        self.turn = Some(sha.clone());
      }
      if let Some(entry) = &mut self.entry {
        entry.sha = sha;
      }
      self.beat_at = Instant::now();
    }
    Ok(pushed)
  }

  /// Whether origin still holds this claim's entry at the sha this claim last wrote, read from origin now.
  async fn entry_holds_lease(&self) -> anyhow::Result<bool> {
    let Some(entry) = &self.entry else {
      return Ok(false);
    };
    let view = self.site.read().await?;
    Ok(view.entries.iter().any(|(_, beat)| beat.refname == entry.refname && beat.sha == entry.sha))
  }

  /// Deletes the refs this claim still holds. A ref that a pruner or a newer head moved is forgotten, and the rest are
  /// deleted in a further push, because the atomic push deleted nothing while one of its leases failed. A refusal
  /// with every held ref still in place is an error.
  pub async fn release(&mut self) -> anyhow::Result<()> {
    loop {
      let changes = self.held_deletes();
      if changes.is_empty() {
        return Ok(());
      }
      match self.site.push(&changes).await? {
        Pushed::Done => {
          self.turn = None;
          self.entry = None;
          return Ok(());
        }
        Pushed::Refused(err) => {
          let view = self.site.read().await?;
          let moved: Vec<&Change> = changes.iter().filter(|change| !self.is_held_in(&view, change)).collect();
          if moved.is_empty() {
            anyhow::bail!("could not release the landing queue refs of {}: {err}", self.holder);
          }
          for change in moved {
            if change.refname() == TURN_REF {
              self.turn = None;
            } else {
              self.entry = None;
            }
          }
        }
      }
    }
  }

  fn held_deletes(&self) -> Vec<Change> {
    let mut changes = Vec::new();
    if let Some(turn) = &self.turn {
      changes.push(Change::Delete { refname: TURN_REF.to_string(), from: turn.clone() });
    }
    if let Some(entry) = &self.entry {
      changes.push(Change::Delete { refname: entry.refname.clone(), from: entry.sha.clone() });
    }
    changes
  }

  fn is_held_in(&self, view: &QueueView, change: &Change) -> bool {
    match change {
      Change::Delete { refname, from } if refname == TURN_REF => {
        view.turn.as_ref().is_some_and(|turn| &turn.sha == from)
      }
      Change::Delete { refname, from } => {
        view.entries.iter().any(|(_, beat)| &beat.refname == refname && &beat.sha == from)
      }
      Change::Create { .. } | Change::Move { .. } => false,
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWake {
  pub id: String,
  pub message: String,
}

impl Owner {
  /// The session's pending triggers on disk, with their labels.
  pub async fn pending(&self) -> anyhow::Result<Vec<PendingWake>> {
    let triggers_dir = layout::triggers_dir(&self.session_dir);
    if !triggers_dir.is_dir() {
      return Ok(Vec::new());
    }
    let mut wakes = Vec::new();
    for (path, _) in trigger_files::iter_trigger_file_stats(&triggers_dir)? {
      let stored = match read_metadata_value(&path) {
        Ok(stored) => stored,
        Err(error) => {
          // A trigger file the server is still writing reads as unreadable for a moment; the next read sees it whole.
          tracing::warn!(target: "charliebot_tasks", path = %path.display(), error = %error, "land_queue_trigger_unreadable");
          continue;
        }
      };
      if stored.get("status").and_then(Value::as_str) != Some("pending") {
        continue;
      }
      let id =
        stored.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{} has no id", path.display()))?;
      let message = stored.get("message").and_then(Value::as_str).unwrap_or_default();
      wakes.push(PendingWake { id: id.to_string(), message: message.to_string() });
    }
    Ok(wakes)
  }

  /// Registers a wake that runs `cmd` detached and wakes the session when it exits.
  pub async fn register(&self, message: &str, cmd: &str) -> anyhow::Result<()> {
    let max_wait = WAKE_MAX_WAIT_SECS.to_string();
    self.charliebot(&["session", "trigger", "--max-wait", &max_wait, "--message", message, "--cmd", cmd]).await
  }

  /// Cancels one pending wake of the session.
  pub async fn cancel(&self, id: &str) -> anyhow::Result<()> {
    self.charliebot(&["session", "trigger", "cancel", id]).await
  }

  async fn charliebot(&self, args: &[&str]) -> anyhow::Result<()> {
    run_program(&self.program, &self.worktree, args).await.map(|_| ())
  }
}

async fn run_program(program: &Path, worktree: &Path, args: &[&str]) -> anyhow::Result<String> {
  let mut command = tokio::process::Command::new(program);
  command.args(args).current_dir(worktree).kill_on_drop(true);
  let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
    .await
    .map_err(|_| anyhow::anyhow!("charliebot {} timed out", args.join(" ")))?
    .with_context(|| format!("charliebot {} failed to start", args.join(" ")))?;
  if !output.status.success() {
    anyhow::bail!(
      "charliebot {} failed (exit {}): {}",
      args.join(" "),
      output.status.code().unwrap_or(-1),
      String::from_utf8_lossy(&output.stderr).trim()
    );
  }
  Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What `arm_landing` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Armed {
  /// A queued command already serves this head: nothing new was registered.
  Existing,
  /// A command was registered for this head; any older head's command and entry were replaced.
  Registered,
}

/// Arms the queued command for a landing whose head `sha` passed its checks. A repeat call for the head a command
/// already serves returns `Existing`; a new head cancels the older head's wake and removes its entry before the new
/// command registers.
pub async fn arm_landing(site: &Site, owner: &Owner, pr: u64, sha: &str) -> anyhow::Result<Armed> {
  let wakes = owner.pending().await?;
  if queued_command_pending(&wakes, sha) {
    return Ok(Armed::Existing);
  }
  for wake in wakes.iter().filter(|wake| is_landing_wake(&wake.message)) {
    owner.cancel(&wake.id).await?;
  }
  let view = site.read().await?;
  if let Some((name, beat)) = view.entry_of(pr, &owner.token()?)
    && beat.parent != sha
  {
    let change = Change::Delete { refname: name.refname(), from: beat.sha.clone() };
    if let Pushed::Refused(err) = site.push(&[change]).await? {
      anyhow::bail!("could not remove the entry of the replaced head of PR {pr}: {}: {err}", name.refname());
    }
  }
  let cmd = format!("timeout {WAKE_MAX_WAIT_SECS} charliebot land queue run --sha {sha}");
  owner.register(&landing_wake_message(sha), &cmd).await?;
  Ok(Armed::Registered)
}

/// Whether `wakes` holds the queued command of the landing head `sha`. This is the one rule that decides whether a
/// queued command is live: `land` returns its waiting record unchanged while the rule holds, and `arm_landing`
/// registers nothing.
pub fn queued_command_pending(wakes: &[PendingWake], sha: &str) -> bool {
  let message = landing_wake_message(sha);
  wakes.iter().any(|wake| wake.message == message)
}

/// The label of the queued command of the landing head `sha`.
fn landing_wake_message(sha: &str) -> String {
  format!("{WAKE_PREFIX}{}", short_sha(sha))
}

fn is_landing_wake(message: &str) -> bool {
  message.starts_with(WAKE_PREFIX) && !message.starts_with(PR_WAKE_PREFIX)
}

/// The label of a pull request merge's queued command.
pub fn pr_merge_wake(pr: u64) -> String {
  format!("{PR_WAKE_PREFIX}{pr}")
}

/// The command a pull request merge's queued wake runs: the merge command under the turn.
pub fn pr_merge_wake_cmd(pr: u64, merge_cmd: &str) -> String {
  format!(
    "timeout {WAKE_MAX_WAIT_SECS} charliebot land queue run --pr-merge {pr} --merge-cmd {}",
    shell_word(merge_cmd)
  )
}

/// The result of the pull request merge gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeGate {
  /// The caller already holds the turn (`TURN_ENV` names it and the queue agrees): it merges now.
  Held,
  /// The merge ran under a turn this call took, and its command exited with this status.
  Ran(i32),
  /// The merge waits in the queue behind the named entry; a wake runs it.
  Queued,
}

/// Runs `merge_cmd` under the turn, keeping the heartbeat while it runs, and releases the turn after it exits.
async fn run_under_turn(claim: &mut Claim, merge_cmd: &str, cadence: Cadence) -> anyhow::Result<i32> {
  let mut command = tokio::process::Command::new("sh");
  command.arg("-c").arg(merge_cmd).env(TURN_ENV, &claim.holder).kill_on_drop(true);
  let mut child = command.spawn().with_context(|| format!("could not start the merge command: {merge_cmd}"))?;
  let status = loop {
    tokio::select! {
      status = child.wait() => break status.context("the merge command failed to report its status")?,
      _ = tokio::time::sleep(cadence.poll) => {
        if claim.beat_due(cadence) {
          let parent = claim.parent.clone();
          if let Pushed::Refused(err) = claim.beat(&parent).await? {
            child.kill().await.context("could not stop the merge command after its turn was lost")?;
            anyhow::bail!("the turn of {} was lost while its merge ran: {err}", claim.holder);
          }
        }
      }
    }
  };
  claim.release().await?;
  Ok(status.code().unwrap_or(-1))
}

/// The pull request merge gate that `github-post.sh pr-merge` calls after its checks. Under `TURN_ENV`, it verifies the
/// turn the caller runs under. Otherwise it takes the turn when no holder and no waiting entry exists and runs
/// `merge_cmd` under it; when the queue is busy it waits behind the entries, registering the queued command.
#[allow(clippy::too_many_arguments, reason = "each argument is one fact of the merge gate's call")]
pub async fn pr_merge_gate(
  site: &Site,
  owner: Option<&Owner>,
  pr: u64,
  head: &str,
  merge_cmd: Option<&str>,
  turn_env: Option<&str>,
  cadence: Cadence,
  ci_passed_ms: u64,
) -> anyhow::Result<MergeGate> {
  if let Some(holder) = turn_env {
    let view = site.read().await?;
    return match &view.turn {
      Some(turn) if turn.holder == holder => Ok(MergeGate::Held),
      Some(turn) => {
        anyhow::bail!("{TURN_ENV} names {holder}, but the turn is held by {}; nothing was merged", turn.holder)
      }
      None => anyhow::bail!("{TURN_ENV} names {holder}, but no turn is held; nothing was merged"),
    };
  }
  let merge_cmd = merge_cmd.ok_or_else(|| anyhow::anyhow!("pr-merge needs its merge command to queue or run it"))?;
  let mut view = site.read().await?;
  if may_take(&view, None) {
    let mut claim = Claim::direct(site, pr, head);
    if claim.take_turn().await? == Take::Taken {
      let status = run_under_turn(&mut claim, merge_cmd, cadence).await?;
      return Ok(MergeGate::Ran(status));
    }
    view = site.read().await?;
  }
  let owner = owner
    .ok_or_else(|| anyhow::anyhow!("pr-merge refuses to enqueue without CHARLIEBOT_SESSION_ID; nothing was merged"))?;
  let key = owner.token()?;
  let mut replaced = false;
  if let Some((name, beat)) = view.entry_of(pr, &key)
    && beat.parent != head
  {
    let change = Change::Delete { refname: name.refname(), from: beat.sha.clone() };
    if let Pushed::Refused(err) = site.push(&[change]).await? {
      anyhow::bail!("could not replace the entry of the older head of PR {pr}: {}: {err}", name.refname());
    }
    for wake in owner.pending().await?.iter().filter(|wake| wake.message == pr_merge_wake(pr)) {
      owner.cancel(&wake.id).await?;
    }
    replaced = true;
  }
  if replaced || view.entry_of(pr, &key).is_none() {
    let name = EntryName::new(ci_passed_ms, pr, &key);
    Claim::enter(site, &name, head).await?;
  }
  let message = pr_merge_wake(pr);
  if !owner.pending().await?.iter().any(|wake| wake.message == message) {
    owner.register(&message, &pr_merge_wake_cmd(pr, merge_cmd)).await?;
  }
  Ok(MergeGate::Queued)
}

/// The queued command of a pull request merge: waits for its entry's turn, runs the merge command under it, releases.
pub async fn run_pr_merge_queued(
  site: &Site,
  owner: &Owner,
  pr: u64,
  merge_cmd: &str,
  cadence: Cadence,
) -> anyhow::Result<i32> {
  let mut claim = claim_of_entry(site, &owner.token()?, pr).await?;
  wait_for_turn(&mut claim, cadence).await?;
  run_under_turn(&mut claim, merge_cmd, cadence).await
}

/// The claim of the live waiting entry that the owner key `owner` holds for `pr`, which a queued command resumes.
pub async fn claim_of_entry(site: &Site, owner: &str, pr: u64) -> anyhow::Result<Claim> {
  let view = site.read().await?;
  let (name, beat) = view
    .entry_of(pr, owner)
    .ok_or_else(|| anyhow::anyhow!("PR {pr} has no waiting entry for {owner}; nothing was merged"))?;
  Ok(Claim::from_beat(site, &view, &name.refname(), beat))
}

/// What a foreground wait for the turn found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquired {
  /// The claim holds the turn.
  Held,
  /// The wait reached its bound; the entry was removed.
  BoundReached,
}

/// The foreground wait of a round's pull request merge: enters or resumes the entry of `owner` for `pr` at the head's
/// CI pass time, keeps its heartbeat, and returns `Held` once it holds the turn or `BoundReached` when `bound` runs out
/// and it has removed the entry. `owner` is the round's launch id, which the entry carries as it is. No server is
/// involved, so this runs on a host without one.
pub async fn acquire(
  site: &Site,
  owner: &str,
  pr: u64,
  head: &str,
  ci_passed_ms: u64,
  bound: Duration,
  cadence: Cadence,
) -> anyhow::Result<Acquired> {
  let view = site.read().await?;
  let name = match view.entry_of(pr, owner) {
    Some((existing, _)) => existing.clone(),
    None => EntryName::new(ci_passed_ms, pr, owner),
  };
  let mut claim = Claim::enter_or_resume(site, &name, head).await?;
  if claim.holds_turn() {
    return Ok(Acquired::Held);
  }
  let started = Instant::now();
  loop {
    if claim.beat_due(cadence) {
      let parent = claim.parent.clone();
      if let Pushed::Refused(err) = claim.beat(&parent).await? {
        // A prune after a stalled push removes an entry whose heartbeat went stale; the waiter keeps its place.
        eprintln!("{} was removed while it waited ({err}); re-entered at its original key {owner}", name.refname());
        claim = Claim::enter(site, &name, head).await?;
        continue;
      }
    }
    let view = site.read().await?;
    if may_take(&view, claim.entry_refname()) && claim.take_turn().await? == Take::Taken {
      return Ok(Acquired::Held);
    }
    let remaining = bound.saturating_sub(started.elapsed());
    if remaining.is_zero() {
      claim.release().await?;
      return Ok(Acquired::BoundReached);
    }
    tokio::time::sleep(remaining.min(cadence.poll)).await;
  }
}

/// Removes the queue refs of the owner key `owner` for `pr`: its waiting entry and, when the turn belongs to it, the
/// turn. A ref another client moved is left in place; a refusal with this owner's refs still in place is an error.
pub async fn release(site: &Site, owner: &str, pr: u64) -> anyhow::Result<()> {
  let view = site.read().await?;
  let mut claim = match view.entry_of(pr, owner) {
    Some((name, beat)) => Claim::from_beat(site, &view, &name.refname(), beat),
    None => Claim {
      site: site.clone(),
      holder: String::new(),
      parent: String::new(),
      entry: None,
      turn: None,
      beat_at: Instant::now(),
    },
  };
  if claim.turn.is_none()
    && let Some(turn) = &view.turn
    && turn_belongs_to(&turn.holder, pr, owner)
  {
    claim.holder = turn.holder.clone();
    claim.parent = turn.parent.clone();
    claim.turn = Some(turn.sha.clone());
  }
  if claim.entry.is_none() && claim.turn.is_none() {
    eprintln!("nothing to release: PR {pr} holds no queue ref for {owner}");
    return Ok(());
  }
  claim.release().await
}

/// The head sha of PR `pr` on `repo`, as GitHub reports it.
pub async fn pr_head(worktree: &Path, repo: &str, pr: u64) -> anyhow::Result<String> {
  let args = vec![
    "pr".to_string(),
    "view".to_string(),
    pr.to_string(),
    "--repo".to_string(),
    repo.to_string(),
    "--json".to_string(),
    "headRefOid".to_string(),
    "--jq".to_string(),
    ".headRefOid".to_string(),
  ];
  let output = run_gh(worktree, &args).await?;
  if !output.success {
    anyhow::bail!("gh pr view {pr} of {repo} failed: {}", output.stderr.trim());
  }
  let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
  if head.is_empty() {
    anyhow::bail!("gh pr view {pr} of {repo} reported no head sha");
  }
  Ok(head)
}

/// One step toward the turn: renews the heartbeat when it is due, then takes the turn when the queue lets this claim.
/// A refused renewal is an error only when origin no longer holds the claim's entry at the sha it last wrote. A refusal
/// that leaves the entry in place is a failed push, and the claim keeps waiting at its key.
pub async fn turn_step(claim: &mut Claim, cadence: Cadence) -> anyhow::Result<Step> {
  if claim.beat_due(cadence) {
    let parent = claim.parent.clone();
    if let Pushed::Refused(err) = claim.beat(&parent).await? {
      if !claim.entry_holds_lease().await? {
        anyhow::bail!("the queue entry of {} was removed while it waited: {err}", claim.holder);
      }
      eprintln!(
        "heartbeat of {} was refused while its entry still holds its sha; it keeps waiting: {err}",
        claim.holder
      );
    }
  }
  let view = claim.site.read().await?;
  if may_take(&view, claim.entry_refname()) && claim.take_turn().await? == Take::Taken {
    return Ok(Step::Held);
  }
  Ok(Step::Waiting)
}

/// Polls until the claim holds the turn.
pub async fn wait_for_turn(claim: &mut Claim, cadence: Cadence) -> anyhow::Result<()> {
  while turn_step(claim, cadence).await? == Step::Waiting {
    tokio::time::sleep(cadence.poll).await;
  }
  Ok(())
}

/// The CI pass time of a head sha in milliseconds: the latest completed check run on it, or `now` when none completed.
pub async fn check_passed_ms(worktree: &Path, repo: &str, sha: &str) -> anyhow::Result<u64> {
  let args = vec!["api".to_string(), format!("repos/{repo}/commits/{sha}/check-runs?per_page=100")];
  let output = run_gh(worktree, &args).await?;
  if !output.success {
    anyhow::bail!("gh api check runs of {sha} failed: {}", output.stderr.trim());
  }
  let listed: Value =
    serde_json::from_slice(&output.stdout).with_context(|| format!("gh api check runs of {sha} printed no JSON"))?;
  let latest = listed
    .get("check_runs")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|run| run.get("completed_at").and_then(Value::as_str))
    .max()
    .map(str::to_string);
  match latest {
    Some(completed_at) => {
      let at = OffsetDateTime::parse(&completed_at, &Rfc3339)
        .with_context(|| format!("a check run of {sha} has a bad completed_at {completed_at}"))?;
      u64::try_from(at.unix_timestamp_nanos() / 1_000_000).context("a check run completed before 1970")
    }
    None => u64::try_from(now_secs() * 1000).context("the clock reads before 1970"),
  }
}

/// The `owner/repo` that the worktree's `origin` names.
pub async fn origin_repo(worktree: &Path) -> anyhow::Result<String> {
  let url = git_ok(worktree, &["remote", "get-url", "origin"], GIT_TIMEOUT, "git remote get-url").await?;
  crate::git::repo_from_remote_url(&url).ok_or_else(|| anyhow::anyhow!("origin's URL {url} names no owner/repo path"))
}

async fn git_ok(worktree: &Path, args: &[&str], timeout: Duration, label: &str) -> anyhow::Result<String> {
  let (ok, out, err) = git::git_stdout(worktree, args, timeout, label).await;
  if !ok {
    anyhow::bail!("{label} failed in {}: {err}", worktree.display());
  }
  Ok(out)
}

fn now_secs() -> i64 {
  OffsetDateTime::now_utc().unix_timestamp()
}

fn short_sha(sha: &str) -> &str {
  &sha[..sha.len().min(12)]
}

/// A POSIX shell word for `text`, so a command line can carry it as one argument.
fn shell_word(text: &str) -> String {
  format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn entry_names_round_trip_and_sort_by_ci_pass_then_pr() {
    let late = EntryName::new(1_795_000_000_500, 5530, "sess-a");
    let early_high_pr = EntryName::new(1_795_000_000_000, 5531, "sess-b");
    let early_low_pr = EntryName::new(1_795_000_000_000, 999, "sess-c");
    assert_eq!(late.refname(), "refs/land-queue/1795000000500-5530-sess-a");
    assert_eq!(EntryName::parse(&early_high_pr.refname()).unwrap(), early_high_pr);
    let mut names = vec![late.clone(), early_high_pr.clone(), early_low_pr.clone()];
    names.sort();
    assert_eq!(names, vec![early_low_pr, early_high_pr, late]);
  }

  #[test]
  fn a_malformed_entry_name_is_an_error() {
    assert!(EntryName::parse("refs/land-queue/179500000050-5530-sess").is_err());
    assert!(EntryName::parse("refs/land-queue/1795000000500-5530-").is_err());
    assert!(EntryName::parse("refs/land-queue/1795000000500-x-sess").is_err());
    assert!(EntryName::parse("refs/heads/main").is_err());
  }

  #[test]
  fn a_session_token_names_no_session_and_keeps_two_sessions_apart() {
    let session = "5e551011-6f0a-4c1d-9b2e-0a1b2c3d4e5f";
    let token = owner_token(session).expect("a session id names a token");
    assert_eq!(token.len(), 16, "the token is 16 hex digits: {token}");
    assert!(token.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)), "lowercase hex: {token}");
    assert_eq!(owner_token(session).expect("the same id names the same token"), token);
    let other = owner_token("0d0e1e2a-7b3c-4d4e-8f5a-6b7c8d9e0f1a").expect("a session id names a token");
    let first = EntryName::new(1_795_000_000_000, 5530, &token);
    let second = EntryName::new(1_795_000_000_000, 5530, &other);
    assert!(!first.refname().contains(session), "the entry names no session id: {}", first.refname());
    assert_ne!(first.refname(), second.refname(), "two sessions with the same PR timing get distinct entries");
    assert_eq!(EntryName::parse(&first.refname()).expect("the entry parses"), first);
    assert!(owner_token("").is_err(), "an empty session id names no entry");
  }

  #[test]
  fn a_round_entry_keeps_its_launch_id_as_it_is() {
    let launch = "20261127T083000Z-12345-23456";
    let entry = EntryName::new(1_795_000_000_000, 5530, launch);
    assert_eq!(entry.refname(), "refs/land-queue/1795000000000-5530-20261127T083000Z-12345-23456");
    assert_eq!(EntryName::parse(&entry.refname()).expect("the entry parses"), entry);
  }

  #[test]
  fn the_owner_check_accepts_its_own_token_and_refuses_another() {
    let mine = owner_token("5e551011-6f0a-4c1d-9b2e-0a1b2c3d4e5f").expect("a session id names a token");
    let theirs = owner_token("0d0e1e2a-7b3c-4d4e-8f5a-6b7c8d9e0f1a").expect("a session id names a token");
    let entry = EntryName::new(1_795_000_000_000, 5530, &mine);
    let holder = entry.refname();
    assert!(turn_belongs_to(&holder, 5530, &mine), "the holder's own token accepts it");
    assert!(!turn_belongs_to(&holder, 5530, &theirs), "another session's token is refused");
    assert!(!turn_belongs_to(&holder, 5531, &mine), "the same owner on another PR holds another turn");
    assert!(!turn_belongs_to("direct pr 5530", 5530, &mine), "a direct writer's holder belongs to no owner");
    let beat =
      Beat { refname: holder.clone(), sha: "a".repeat(40), parent: "b".repeat(40), beat_at: 0, holder: holder.clone() };
    let view = QueueView { turn: None, entries: vec![(entry, beat)] };
    assert!(view.entry_of(5530, &mine).is_some(), "the owner finds its own entry");
    assert!(view.entry_of(5530, &theirs).is_none(), "another owner finds no entry of this one");
  }

  #[test]
  fn the_turn_goes_to_the_earliest_entry_only_when_free() {
    let beat = |name: &EntryName| Beat {
      refname: name.refname(),
      sha: "a".repeat(40),
      parent: "b".repeat(40),
      beat_at: 0,
      holder: name.refname(),
    };
    let first = EntryName::new(1, 10, "s1");
    let second = EntryName::new(2, 11, "s2");
    let view = QueueView { turn: None, entries: vec![(first.clone(), beat(&first)), (second.clone(), beat(&second))] };
    assert!(may_take(&view, Some(&first.refname())));
    assert!(!may_take(&view, Some(&second.refname())));
    assert!(!may_take(&view, None), "a direct writer waits behind a waiting entry");
    let empty = QueueView { turn: None, entries: Vec::new() };
    assert!(may_take(&empty, None), "a direct writer takes a free turn with no entry");
    let held = QueueView { turn: Some(beat(&first)), entries: Vec::new() };
    assert!(!may_take(&held, None));
  }

  #[test]
  fn a_landing_wake_excludes_the_merge_wakes() {
    assert!(is_landing_wake("land queue 0123456789ab"));
    assert!(!is_landing_wake("land queue pr 5530"));
    assert!(!is_landing_wake("ci gate 0123456789ab"));
  }

  #[test]
  fn a_merge_command_survives_its_shell_word() {
    assert_eq!(shell_word("a 'b' c"), "'a '\\''b'\\'' c'");
  }

  #[test]
  #[allow(
    clippy::assertions_on_constants,
    reason = "the two production bounds are constants, and this test pins their order"
  )]
  fn the_round_wait_bound_stays_below_the_round_tool_call_limit() {
    assert!(
      ROUND_WAIT_BOUND_SECS < ROUND_TOOL_CALL_SECS,
      "a round waits {ROUND_WAIT_BOUND_SECS}s for the turn inside a {ROUND_TOOL_CALL_SECS}s tool call"
    );
  }
}
