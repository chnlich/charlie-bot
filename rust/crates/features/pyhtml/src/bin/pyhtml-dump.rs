use std::error::Error;
use std::io::{self, BufRead, Read};
use std::process::ExitCode;

use charliebot_pyhtml::parse;

fn main() -> ExitCode {
  match run() {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      eprintln!("{error}");
      ExitCode::FAILURE
    }
  }
}

fn run() -> Result<(), Box<dyn Error>> {
  let mut convert_charrefs = true;
  let mut mode_was_set = false;
  let mut batch_jsonl = false;
  for argument in std::env::args().skip(1) {
    match argument.as_str() {
      "--convert-charrefs" => {
        if mode_was_set {
          return Err("choose one character-reference mode".into());
        }
        convert_charrefs = true;
        mode_was_set = true;
      }
      "--no-convert-charrefs" => {
        if mode_was_set {
          return Err("choose one character-reference mode".into());
        }
        convert_charrefs = false;
        mode_was_set = true;
      }
      "--batch-jsonl" => batch_jsonl = true,
      _ => return Err(format!("unknown argument: {argument}").into()),
    }
  }

  if batch_jsonl {
    for line in io::stdin().lock().lines() {
      let source: String = serde_json::from_str(&line?)?;
      let events = parse(&source, convert_charrefs)?;
      println!("{}", serde_json::to_string(&events)?);
    }
  } else {
    let mut source = String::new();
    io::stdin().read_to_string(&mut source)?;
    for event in parse(&source, convert_charrefs)? {
      println!("{}", serde_json::to_string(&event)?);
    }
  }
  Ok(())
}
