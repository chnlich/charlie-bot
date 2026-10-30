//! The `policy` command (`src/runtime/cli/policy.py`): print the approval policy table as the server reads it.
//!
//! ```text
//! charliebot policy show
//! ```
//!
//! The table loads through the same loader the server's prompt assembly and web routes use, so the output is the
//! validated table and an absent or invalid table is the same error the server refuses with. Stdout carries the
//! titled section the manager prompts carry, the table rendered back to YAML; diagnostics go to stderr.

use std::ffi::OsString;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::approval_policy;
use charliebot_core::home;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "policy", about = "Print the approval policy table as the server reads it")]
struct PolicyCli {
  #[command(subcommand)]
  verb: PolicyVerb,
}

#[derive(Subcommand)]
enum PolicyVerb {
  /// Print the approval policy table as the manager prompts carry it (nothing else)
  Show,
}

/// Parse the policy arguments and run one verb. Parser errors print and exit on their own (argparse-compatible:
/// help 0, usage error 2).
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(PolicyCli::command());
  let argv = std::iter::once(OsString::from("charliebot policy")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = PolicyCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.verb {
    PolicyVerb::Show => show(),
  }
}

fn show() -> Result<(), CliExit> {
  let home = home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  show_in_home(&home)
}

fn show_in_home(home: &std::path::Path) -> Result<(), CliExit> {
  let loaded = approval_policy::load(home).map_err(|error| CliExit::error(error.to_string()))?;
  print!("{}", approval_policy::prompt_section(&loaded.table));
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::show_in_home;

  const VALID_TABLE: &str = "\
consequence_tests:
  - {id: irreversible, rank: 1, text: \"Deletes data with no backup.\"}
  - {id: chao-name, rank: 2, text: \"A message in Chao's name reaches other people.\"}
  - {id: money, rank: 3, text: \"Spends money beyond a budget that Chao set.\"}
  - {id: authority, rank: 4, text: \"Widens what agents may do.\"}
  - {id: rule-named, rank: 4, text: \"A rule names the user's explicit approval.\"}
  - {id: reading, rank: 5, text: \"Picks a reading for work over about one worker-day.\"}
standing_policies: []
enable_ask_tier_refusal: false
";

  fn write_table(home: &std::path::Path, text: &str) {
    let table = home.join("config.d").join("approval_policy.yaml");
    fs::create_dir_all(table.parent().unwrap()).unwrap();
    fs::write(table, text).unwrap();
  }

  #[test]
  fn show_refuses_an_absent_table_with_the_path_and_prints_a_valid_one() {
    let home = tempfile::tempdir().unwrap();

    let error = show_in_home(home.path()).expect_err("the absent table is an error");

    assert!(error.body.to_string().contains("config.d/approval_policy.yaml: missing"), "{}", error.body);

    write_table(home.path(), "consequence_tests: []\nstanding_policies: []\nenable_ask_tier_refusal: false\n");
    let error = show_in_home(home.path()).expect_err("an invalid table is an error");
    assert!(error.body.to_string().contains("consequence_tests: consequence test id"), "{}", error.body);

    write_table(home.path(), VALID_TABLE);
    show_in_home(home.path()).expect("a valid table prints");
  }
}
