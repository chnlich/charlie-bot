//! The `ci-wait` verb: the CI gate's registration wait.
//!
//! `charliebot land` registers a trigger whose command runs this verb and then `gh pr checks --watch`. The verb
//! exits 0 once the head sha has a check run or a commit status (`land::await_check_registration`). A rate-limit
//! 403 is printed with its reset time on stderr, and the verb waits until that time before it polls again.

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use clap::{Arg, Command};

const ABOUT: &str = "Wait until a head sha has a check run or a commit status, then exit 0; a rate-limit 403 waits \
     until its reset time";

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(
    Command::new("ci-wait")
      .about(ABOUT)
      .arg(Arg::new("repo").long("repo").value_name("OWNER/REPO").required(true).help("Repository of the head sha"))
      .arg(Arg::new("sha").long("sha").value_name("SHA").required(true).help("Head sha whose registration is awaited")),
  );
  let matches = match command
    .clone()
    .try_get_matches_from(std::iter::once(OsString::from("charliebot ci-wait")).chain(args.iter().cloned()))
  {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let repo = matches.get_one::<String>("repo").expect("clap requires --repo");
  let sha = matches.get_one::<String>("sha").expect("clap requires --sha");
  let workdir =
    std::env::current_dir().map_err(|error| CliExit::error(format!("cannot read the working directory: {error}")))?;
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| CliExit::error(format!("tokio runtime failed to start: {error}")))?;
  let workdir = workdir.as_path();
  let run = move |args: Vec<String>| async move { charliebot_tasks::github_api::run_gh(workdir, &args).await };
  runtime
    .block_on(charliebot_tasks::land::await_check_registration(
      &run,
      repo,
      sha,
      charliebot_tasks::land::CHECK_REGISTRATION_POLL,
      &|line: &str| eprintln!("{line}"),
    ))
    .map_err(|error| CliExit::error(format!("{error:#}")))
}
