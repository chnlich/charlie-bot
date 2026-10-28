//! The stand-in for a release's `charliebot` binary in the switch tests: it serves `switch-helper`, the one
//! subcommand `scripts/switch-server.sh` calls, through the code the real CLI dispatches to.

use std::ffi::OsString;
use std::process::exit;

fn main() {
  let args: Vec<OsString> = std::env::args_os().skip(1).collect();
  if args.first().and_then(|argument| argument.to_str()) != Some("switch-helper") {
    eprintln!("charliebot: unknown subcommand");
    exit(2);
  }
  match charliebot_switch_helper::main(&args[1..]) {
    Ok(()) => exit(0),
    Err(failure) => {
      match serde_json::to_string(&failure.body) {
        Ok(body) => eprintln!("{body}"),
        Err(error) => eprintln!("cannot render the failure: {error}"),
      }
      exit(failure.code);
    }
  }
}
