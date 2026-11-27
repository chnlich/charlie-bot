//! The `land queue` verbs: the landing queue on origin (`charliebot_tasks::land_queue`).
//!
//! `list` prints the queue in its order. `run --sha` is the command a landing's wake runs once CI is awaited, and
//! `run --pr-merge` the one a queued merge runs; both finish the landing or the merge before they exit. `pr-merge` is
//! the gate that `github-post.sh pr-merge` calls outside a round: it exits 0 when the caller holds the turn or its
//! merge ran, and 75 (`queued`) when the merge waits behind the landing queue and a wake runs it. `acquire` and
//! `release` are the server-free verbs a round's merge calls: `acquire` waits for the turn with a heartbeat and a
//! bound, and `release` removes the turn and the entry after the merge.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::{CliExit, charliebot_home_or_exit};
use charliebot_cli_common::json_out;
use charliebot_cli_common::resolve;
use charliebot_core::config;
use charliebot_core::run_token::SESSION_ID_ENV;
use charliebot_tasks::land::{LandState, QueueEnd};
use charliebot_tasks::land_queue::{self, Cadence, Owner, Site};
use clap::{Arg, ArgAction, Command};
use serde_json::json;

/// The exit code of a `run --sha` whose head moved on: a newer landing call owns the landing now.
const SUPERSEDED_EXIT: i32 = 9;
/// The exit code of a `pr-merge` that queued, and of an `acquire` that reached its bound without the turn.
const QUEUED_EXIT: i32 = 75;

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let Some((verb, rest)) = args.split_first() else {
    return Err(CliExit::error("usage: charliebot land queue list | run | pr-merge | acquire | release"));
  };
  match verb.to_str() {
    Some("list") => list(rest),
    Some("run") => run(rest),
    Some("pr-merge") => pr_merge(rest),
    Some("acquire") => acquire(rest),
    Some("release") => release(rest),
    _ => Err(CliExit::error(format!(
      "unknown land queue verb {}: use list, run, pr-merge, acquire or release",
      verb.to_string_lossy()
    ))),
  }
}

fn parse(
  name: &'static str,
  about: &'static str,
  args: &[OsString],
  command: Command,
) -> Result<clap::ArgMatches, CliExit> {
  let command = with_infer_long_args(command.name(name).about(about));
  match command.clone().try_get_matches_from(
    std::iter::once(OsString::from(format!("charliebot land queue {name}"))).chain(args.iter().cloned()),
  ) {
    Ok(matches) => Ok(matches),
    Err(error) => error.exit(),
  }
}

fn runtime() -> Result<tokio::runtime::Runtime, CliExit> {
  tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(format!("tokio runtime failed to start: {error}")))
}

fn working_dir() -> Result<PathBuf, CliExit> {
  std::env::current_dir().map_err(|error| CliExit::error(format!("cannot read the working directory: {error}")))
}

/// The owner a session names: its waiting entries and its wakes.
fn owner_of(session_id: String, worktree: &Path) -> Result<Owner, CliExit> {
  let home = charliebot_home_or_exit()?;
  let session_dir = config::sessions_dir_in(&home).join(&session_id);
  Ok(Owner { session_id, session_dir, worktree: worktree.to_path_buf(), program: PathBuf::from("charliebot") })
}

/// `CHARLIEBOT_SESSION_ID` when the run carries one; an empty value is no session.
fn env_session() -> Option<String> {
  std::env::var(SESSION_ID_ENV).ok().filter(|value| !value.is_empty())
}

fn pr_arg(matches: &clap::ArgMatches) -> Result<u64, CliExit> {
  matches
    .get_one::<String>("pr")
    .ok_or_else(|| CliExit::error("--pr is required"))?
    .parse()
    .map_err(|error| CliExit::error(format!("--pr takes a PR number: {error}")))
}

fn list(args: &[OsString]) -> Result<(), CliExit> {
  parse(
    "list",
    "Print the landing queue on origin, in order, with the turn holder and each heartbeat's age",
    args,
    Command::new("list"),
  )?;
  let site = Site { worktree: working_dir()? };
  let view = runtime()?
    .block_on(site.read())
    .map_err(|error| CliExit::error(format!("reading the landing queue failed: {error:#}")))?;
  let now = i64::try_from(
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_err(|error| CliExit::error(error.to_string()))?
      .as_secs(),
  )
  .map_err(|error| CliExit::error(error.to_string()))?;
  if let Some(turn) = &view.turn {
    println!("turn {}: held by {}, heartbeat {}s old", land_queue::TURN_REF, turn.holder, now - turn.beat_at);
  }
  if view.entries.is_empty() && view.turn.is_none() {
    println!("the landing queue is empty");
  }
  for (position, (name, beat)) in view.entries.iter().enumerate() {
    println!(
      "{} {} PR {} owner {} ci passed {}s ago, heartbeat {}s old",
      position + 1,
      name.refname(),
      name.pr,
      name.owner,
      now - i64::try_from(name.ci_passed_ms / 1000).unwrap_or(i64::MAX),
      now - beat.beat_at
    );
  }
  Ok(())
}

fn run(args: &[OsString]) -> Result<(), CliExit> {
  let command =
    Command::new("run")
      .arg(Arg::new("sha").long("sha").value_name("SHA").conflicts_with("pr-merge").help(
        "Head sha of a landing: the command waits for its CI, queues it, takes the turn, rebases, and writes main",
      ))
      .arg(Arg::new("pr-merge").long("pr-merge").value_name("PR").conflicts_with("sha").help(
        "PR of a queued merge: the command takes the turn its entry waits for and runs the merge command under it",
      ))
      .arg(
        Arg::new("merge-cmd")
          .long("merge-cmd")
          .value_name("CMD")
          .requires("pr-merge")
          .help("The shell command that merges under the turn; it carries the facts of the call that queued the merge"),
      );
  let matches =
    parse("run", "Finish a queued landing or merge; the wake that the queue registered runs it", args, command)?;
  let worktree = working_dir()?;
  let session_id = resolve::resolve_from_env(None)?;
  if let Some(sha) = matches.get_one::<String>("sha") {
    return run_landing(&session_id, sha);
  }
  let pr: u64 = matches
    .get_one::<String>("pr-merge")
    .ok_or_else(|| CliExit::error("land queue run needs --sha or --pr-merge"))?
    .parse()
    .map_err(|error| CliExit::error(format!("--pr-merge takes a PR number: {error}")))?;
  let merge_cmd =
    matches.get_one::<String>("merge-cmd").ok_or_else(|| CliExit::error("--pr-merge needs --merge-cmd"))?;
  let owner = owner_of(session_id, &worktree)?;
  let site = Site { worktree };
  let code = runtime()?
    .block_on(land_queue::run_pr_merge_queued(&site, &owner, pr, merge_cmd, Cadence::PRODUCTION))
    .map_err(|error| CliExit::error(format!("queued merge of PR {pr} failed: {error:#}")))?;
  if code != 0 {
    std::process::exit(code);
  }
  Ok(())
}

fn run_landing(session_id: &str, sha: &str) -> Result<(), CliExit> {
  let home = charliebot_home_or_exit()?;
  let registry = charliebot_packages::registry().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let ci_wait = Duration::from_secs(u64::try_from(charliebot_tasks::land::HOLDER_CI_WAIT_SECS).unwrap_or(u64::MAX));
  let end = runtime()?
    .block_on(charliebot_tasks::land::run_queued(&registry, &home, session_id, sha, Cadence::PRODUCTION, ci_wait))
    .map_err(|error| CliExit::error(format!("landing queue failed: {error:#}")))?;
  match end {
    QueueEnd::Record(record) => {
      let exit_code = LandState::try_from(record.state.as_str())
        .map_err(|error| CliExit::error(format!("landing record holds an unknown state: {error}")))?
        .exit_code();
      json_out::print_result(&serde_json::to_value(&record).map_err(|error| CliExit::error(error.to_string()))?);
      if exit_code != 0 {
        std::process::exit(exit_code);
      }
      Ok(())
    }
    QueueEnd::Superseded => {
      json_out::print_result(&json!({ "state": "superseded", "sha": sha }));
      std::process::exit(SUPERSEDED_EXIT);
    }
  }
}

fn pr_merge(args: &[OsString]) -> Result<(), CliExit> {
  let command = Command::new("pr-merge")
    .arg(Arg::new("pr").long("pr").value_name("PR").required(true).help("The PR to merge"))
    .arg(
      Arg::new("sha")
        .long("sha")
        .value_name("SHA")
        .required(true)
        .help("The PR head the merge reviews; it is the commit the queue entry points at"),
    )
    .arg(
      Arg::new("merge-cmd")
        .long("merge-cmd")
        .value_name("CMD")
        .action(ArgAction::Set)
        .help("The shell command that merges when this call takes the turn or queues the merge"),
    );
  let matches = parse("pr-merge", "Gate a PR merge on the landing queue's turn", args, command)?;
  let pr = pr_arg(&matches)?;
  let head = matches.get_one::<String>("sha").expect("clap requires --sha");
  let merge_cmd = matches.get_one::<String>("merge-cmd").map(String::as_str);
  let turn_env = std::env::var(land_queue::TURN_ENV).ok().filter(|value| !value.is_empty());
  let worktree = working_dir()?;
  let site = Site { worktree: worktree.clone() };
  let owner = env_session().map(|session| owner_of(session, &worktree)).transpose()?;
  let runtime = runtime()?;
  let ci_passed_ms = if turn_env.is_none() {
    runtime
      .block_on(async {
        let repo = land_queue::origin_repo(&worktree).await?;
        land_queue::check_passed_ms(&worktree, &repo, head).await
      })
      .map_err(|error| CliExit::error(format!("reading the CI pass time of {head} failed: {error:#}")))?
  } else {
    0
  };
  let gate = runtime
    .block_on(land_queue::pr_merge_gate(
      &site,
      owner.as_ref(),
      pr,
      head,
      merge_cmd,
      turn_env.as_deref(),
      Cadence::PRODUCTION,
      ci_passed_ms,
    ))
    .map_err(|error| CliExit::error(format!("{error:#}")))?;
  match gate {
    land_queue::MergeGate::Held => Ok(()),
    land_queue::MergeGate::Ran(0) => Ok(()),
    land_queue::MergeGate::Ran(code) => std::process::exit(code),
    land_queue::MergeGate::Queued => {
      eprintln!("queued: PR {pr} waits in the landing queue; its wake runs the merge when the turn comes");
      std::process::exit(QUEUED_EXIT);
    }
  }
}

fn acquire(args: &[OsString]) -> Result<(), CliExit> {
  let command = Command::new("acquire")
    .arg(Arg::new("pr").long("pr").value_name("PR").required(true).help("The PR whose merge waits for the turn"))
    .arg(
      Arg::new("owner")
        .long("owner")
        .value_name("ID")
        .required(true)
        .help("The round's launch id: the entry carries it as it is, and re-entry finds it"),
    )
    .arg(
      Arg::new("bound")
        .long("bound")
        .value_name("SECONDS")
        .value_parser(clap::value_parser!(u64))
        .help("The longest wait; at the bound the entry is removed and the verb exits 75 (default 780)"),
    );
  let matches = parse("acquire", "Wait for the landing queue's turn, keeping this owner's entry alive", args, command)?;
  let pr = pr_arg(&matches)?;
  let owner = matches.get_one::<String>("owner").expect("clap requires --owner").clone();
  let bound = matches
    .get_one::<u64>("bound")
    .copied()
    .or_else(|| std::env::var("CHARLIEBOT_LAND_QUEUE_BOUND_SECS").ok().and_then(|value| value.parse().ok()))
    .unwrap_or(
      u64::try_from(land_queue::ROUND_WAIT_BOUND_SECS).expect("the round wait bound is a positive number of seconds"),
    );
  let worktree = working_dir()?;
  let site = Site { worktree: worktree.clone() };
  let runtime = runtime()?;
  let (head, ci_passed_ms) = runtime
    .block_on(async {
      let repo = land_queue::origin_repo(&worktree).await?;
      let head = land_queue::pr_head(&worktree, &repo, pr).await?;
      let ci_passed_ms = land_queue::check_passed_ms(&worktree, &repo, &head).await?;
      Ok::<_, anyhow::Error>((head, ci_passed_ms))
    })
    .map_err(|error| CliExit::error(format!("reading PR {pr} failed: {error:#}")))?;
  let acquired = runtime
    .block_on(land_queue::acquire(
      &site,
      &owner,
      pr,
      &head,
      ci_passed_ms,
      Duration::from_secs(bound),
      Cadence::from_env(),
    ))
    .map_err(|error| CliExit::error(format!("waiting for the turn of PR {pr} failed: {error:#}")))?;
  match acquired {
    land_queue::Acquired::Held => {
      // The merge command's stdout carries its result, so the queue's own report goes to stderr.
      eprintln!("holding the turn for PR {pr} as {owner}");
      Ok(())
    }
    land_queue::Acquired::BoundReached => {
      eprintln!("PR {pr}: the turn did not come within {bound}s; the entry was removed and nothing was merged");
      std::process::exit(QUEUED_EXIT);
    }
  }
}

fn release(args: &[OsString]) -> Result<(), CliExit> {
  let command = Command::new("release")
    .arg(Arg::new("pr").long("pr").value_name("PR").required(true).help("The PR whose merge held the turn"))
    .arg(
      Arg::new("owner")
        .long("owner")
        .value_name("ID")
        .required(true)
        .help("The round's launch id that the entry and the turn carry, as it is"),
    );
  let matches = parse("release", "Remove this owner's entry and the turn after the merge", args, command)?;
  let pr = pr_arg(&matches)?;
  let owner = matches.get_one::<String>("owner").expect("clap requires --owner").clone();
  let site = Site { worktree: working_dir()? };
  runtime()?
    .block_on(land_queue::release(&site, &owner, pr))
    .map_err(|error| CliExit::error(format!("releasing the turn of PR {pr} failed: {error:#}")))?;
  Ok(())
}
