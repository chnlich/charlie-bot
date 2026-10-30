//! The `digest` command (`src/runtime/cli/digest.py`): record one approval digest entry.
//!
//! ```text
//! charliebot digest add --policy <policy id | t2> --step <text> --undo <text> [--scope-check <text>] [--link <url>]
//! ```
//!
//! The entry lands in `$CHARLIEBOT_HOME/state/digest_entries.jsonl` through the core store, which reads the approval
//! policy table and refuses an unknown policy, an expired policy, a use over the policy's cap in the current Pacific
//! day, and a standing-policy use without `--scope-check`; every refusal names the reason and tells the caller to ask
//! for the step as a T0 Waiting on you item. The calling session comes from the server-written
//! `CHARLIEBOT_SESSION_ID`, an explicit `--session`, or the session directory the command runs in.

use std::ffi::OsString;
use std::path::Path;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::resolve;
use charliebot_core::approval_digest;
use charliebot_core::home;
use charliebot_core::timefmt;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "digest", about = "Record an approval digest entry")]
struct DigestCli {
  #[command(subcommand)]
  verb: DigestVerb,
}

#[derive(Subcommand)]
enum DigestVerb {
  /// Record one T1 policy use or T2 decision in the day's digest
  Add {
    /// The standing policy the step uses, or t2 for a decision the agent tree made
    #[arg(long)]
    policy: String,
    /// What was done
    #[arg(long)]
    step: String,
    /// How to undo it
    #[arg(long)]
    undo: String,
    /// Why the step stays inside the policy's scope; required for a standing policy
    #[arg(long)]
    scope_check: Option<String>,
    /// A link to the step's evidence
    #[arg(long)]
    link: Option<String>,
    /// Session ID (optional; taken from the CHARLIEBOT_SESSION_ID the server writes into the environment)
    #[arg(long)]
    session: Option<String>,
  },
}

/// Parse the digest arguments and run one verb. Parser errors print and exit on their own (argparse-compatible:
/// help 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(DigestCli::command());
  let argv = std::iter::once(OsString::from("charliebot digest")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = DigestCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.verb {
    DigestVerb::Add { policy, step, undo, scope_check, link, session } => {
      let session = resolve::resolve_from_env(session.as_deref())?;
      let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
      add_in_home(&home, &session, &policy, &step, &undo, scope_check.as_deref(), link.as_deref())
    }
  }
}

fn add_in_home(
  home: &Path,
  session: &str,
  policy: &str,
  step: &str,
  undo: &str,
  scope_check: Option<&str>,
  link: Option<&str>,
) -> Result<(), CliExit> {
  let call =
    approval_digest::DigestCall { session, policy, step, undo, scope_check, link, now: &timefmt::utc_now_iso() };
  let entry = approval_digest::add(home, call).map_err(|error| CliExit::error(error.to_string()))?;
  println!("digest entry recorded: policy {}, time {}", entry.policy, entry.time);
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::add_in_home;
  use charliebot_core::approval_digest;
  use charliebot_core::approval_policy::TABLE_RELATIVE_PATH;

  const TABLE: &str = "\
consequence_tests:
  - {id: irreversible, rank: 1, text: \"Deletes data with no backup.\"}
  - {id: chao-name, rank: 2, text: \"A message in Chao's name reaches other people.\"}
  - {id: money, rank: 3, text: \"Spends money beyond a budget that Chao set.\"}
  - {id: authority, rank: 4, text: \"Widens what agents may do.\"}
  - {id: rule-named, rank: 4, text: \"A rule in force names the user's explicit approval.\"}
  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}
standing_policies:
  - {id: shared-repo-landing, scope: \"Land on a shared non-main branch.\", cap_per_pt_day: 16, expires: 2099-01-01}
enable_ask_tier_refusal: false
";

  fn home_with_table() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(TABLE_RELATIVE_PATH);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, TABLE).unwrap();
    home
  }

  #[test]
  fn a_valid_add_records_and_a_refusal_exits_with_the_reason() {
    let home = home_with_table();

    add_in_home(home.path(), "session-a", "shared-repo-landing", "Merged.", "Revert it.", Some("Chao's repo."), None)
      .expect("the use records");
    add_in_home(home.path(), "session-a", "t2", "Chose the layout.", "Re-run with the other.", None, None)
      .expect("the decision records without a scope check");
    assert_eq!(approval_digest::read_entries(home.path()).unwrap().len(), 2);

    let error = add_in_home(home.path(), "session-a", "chaos", "Step.", "Undo.", None, None)
      .expect_err("an unknown policy refuses");
    assert_eq!(error.code, 1);
    assert!(error.body["error"].as_str().unwrap().contains("unknown standing policy"), "{}", error.body);

    let error = add_in_home(home.path(), "session-a", "shared-repo-landing", "Merged.", "Revert it.", None, None)
      .expect_err("a missing scope check refuses");
    assert!(error.body["error"].as_str().unwrap().contains("--scope-check is required"), "{}", error.body);
  }
}
