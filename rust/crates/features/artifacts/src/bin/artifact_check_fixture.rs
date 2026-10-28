use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use charliebot_artifacts::artifact_check::AssertionOutcome;

fn main() -> Result<(), Box<dyn Error>> {
  let mut arguments = std::env::args_os().skip(1);
  let artifact = arguments.next().map(PathBuf::from).ok_or("usage: artifact-check-fixture PAGE_PATH CHROME_PATH")?;
  let chrome_bin = arguments.next().map(PathBuf::from).ok_or("usage: artifact-check-fixture PAGE_PATH CHROME_PATH")?;
  if arguments.next().is_some() {
    return Err("usage: artifact-check-fixture PAGE_PATH CHROME_PATH".into());
  }
  let chrome_bin = chrome_bin.to_string_lossy().into_owned();
  let repo_root =
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).ok_or("the crate lies under the repository")?;
  let home = tempfile::tempdir()?;
  fs::create_dir(home.path().join("sessions"))?;
  let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
  let outcomes = runtime.block_on(charliebot_artifacts::artifact_check::run_assertions_with_chrome(
    "plan",
    artifact,
    chrome_bin,
    repo_root.to_path_buf(),
    charliebot_user_text_checks::UserTextChecksConfig::default(),
    home.path().to_path_buf(),
  ));
  let close = charliebot_artifacts::close_renderer();
  let outcomes = outcomes?;
  close?;
  for outcome in outcomes {
    print_outcome(&outcome)?;
  }
  Ok(())
}

fn print_outcome(outcome: &AssertionOutcome) -> Result<(), serde_json::Error> {
  println!("{}", serde_json::to_string(outcome)?);
  Ok(())
}
