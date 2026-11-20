//! The stand-in for a release's `charliebot` binary in the switch tests: it serves `switch-helper`, the one
//! subcommand `scripts/switch-server.sh` calls, through the code the real CLI dispatches to, and the two `screenshots`
//! reads the switch's capture step makes. `KNOWN_VIEW` is the one View line that `screenshots resolve` passes.

use std::ffi::OsString;
use std::process::exit;

fn main() {
  let args: Vec<OsString> = std::env::args_os().skip(1).collect();
  if args.first().and_then(|argument| argument.to_str()) == Some("screenshots") {
    screenshots(&args[1..]);
  }
  if args.first().and_then(|argument| argument.to_str()) != Some(charliebot_switch_helper::SUBCOMMAND) {
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

/// `sizes` prints one window size. `resolve VIEW` passes only the view `KNOWN_VIEW` names, and refuses any other with
/// a message in the shape the real CLI prints for a view the live server does not resolve.
fn screenshots(args: &[OsString]) -> ! {
  let args: Option<Vec<&str>> = args.iter().map(|argument| argument.to_str()).collect();
  let code = match args.as_deref() {
    Some(["sizes"]) => {
      println!("1536x900");
      0
    }
    Some(["resolve", view]) if std::env::var("KNOWN_VIEW").is_ok_and(|known| known == *view) => 0,
    Some(["resolve", view]) => {
      eprintln!("the View line `{view}` must name a view on the live server, but the live server does not know it");
      1
    }
    _ => {
      eprintln!("charliebot: unknown screenshots command");
      2
    }
  };
  exit(code)
}
