use std::fs;
use std::io;
use std::path::Path;

use charliebot_artifacts::plan_diff;
use serde_json::Value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let arguments: Vec<_> = std::env::args_os().collect();
  if arguments.len() != 3 {
    return Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: artifact-plan-diff INPUT_DIR OUTPUT_DIR").into());
  }
  let input_dir = Path::new(&arguments[1]);
  let output_dir = Path::new(&arguments[2]);
  let mut inputs: Vec<_> =
    fs::read_dir(input_dir)?.map(|entry| entry.map(|entry| entry.path())).collect::<Result<_, _>>()?;
  inputs.sort();
  for input in inputs {
    if input.extension().and_then(|extension| extension.to_str()) != Some("json") {
      continue;
    }
    let pair: Value = serde_json::from_str(&fs::read_to_string(&input)?)?;
    let base = pair
      .get("base")
      .and_then(Value::as_str)
      .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "pair has no text base"))?;
    let new = pair
      .get("new")
      .and_then(Value::as_str)
      .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "pair has no text new page"))?;
    let output = plan_diff::annotate(base, new).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let output_path = output_dir.join(input.file_stem().expect("a pair file has a name")).with_extension("html");
    fs::write(output_path, output)?;
  }
  Ok(())
}
