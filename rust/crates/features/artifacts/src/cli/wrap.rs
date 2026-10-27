//! Assemble a genre template around a body fragment and pre-render its math spans.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_cli_common::exit::CliExit;
use clap::Args;

const KATEX_CDN_URL: &str = "https://cdn.jsdelivr.net/npm/katex@0.16.21/dist/katex.min.js";

#[derive(Args)]
pub(super) struct WrapArgs {
  /// Content fragment path: the page's `<body>` content
  fragment: String,
  /// Genre whose template shells the page
  #[arg(long, value_parser = crate::artifact_check::ARTIFACT_GENRES)]
  genre: String,
  /// Assembled page path
  #[arg(long)]
  output: String,
  /// Pre-render math spans at assembly time
  #[arg(long, action = clap::ArgAction::SetTrue)]
  math: bool,
  /// Keep math source in the assembled page
  #[arg(long = "no-math", action = clap::ArgAction::SetTrue)]
  no_math: bool,
}

impl WrapArgs {
  fn math_enabled(&self, argv: &[OsString]) -> bool {
    let override_value = argv
      .iter()
      .filter_map(|argument| match argument.to_str() {
        Some("--math") => Some(true),
        Some("--no-math") => Some(false),
        _ => None,
      })
      .next_back();
    override_value.unwrap_or_else(|| self.math || (!self.no_math && self.genre == "explain"))
  }
}

/// Run `artifact wrap` and return its stdout line.
pub(super) fn run(args: &WrapArgs, argv: &[OsString]) -> Result<String, CliExit> {
  let fragment_arg = PathBuf::from(&args.fragment);
  let fragment = resolve_path(&fragment_arg).map_err(|error| CliExit::error(error.to_string()))?;
  if !fragment.is_file() {
    return Err(CliExit::usage(format!("fragment not found: {}", args.fragment)));
  }
  let output_arg = PathBuf::from(&args.output);
  let output = resolve_path(&output_arg).map_err(|error| CliExit::error(error.to_string()))?;
  let home = charliebot_core::home::charliebot_home_dir().map_err(|error| CliExit::error(error.to_string()))?;
  let repo_root =
    charliebot_core::repo_root::RepoRoot::new(std::env::var_os(charliebot_core::repo_root::REPO_ROOT_ENV))
      .map_err(|error| CliExit::error(error.to_string()))?;
  let vendor = home.join("vendor").join("katex").join("katex.min.js");
  let node = [OsString::from("node")];
  let written =
    wrap_fragment(&args.genre, &fragment, &output, args.math_enabled(argv), &vendor, repo_root.path(), &node)
      .map_err(CliExit::error)?;
  Ok(format!("wrote {}\n", written.display()))
}

fn wrap_fragment(
  genre: &str,
  fragment: &Path,
  output: &Path,
  math: bool,
  vendor: &Path,
  repo_root: &Path,
  node_command: &[OsString],
) -> Result<PathBuf, String> {
  let template_name = match genre {
    "plan" | "understanding" => "plan_template.html",
    "sitrep" => "sitrep_template.html",
    "debug" => "debug_template.html",
    "explain" => "explain_template.html",
    other => return Err(format!("unknown artifact genre: {other}")),
  };
  let template_rel = format!("prompts/{template_name}");
  let template_path = repo_root.join(&template_rel);
  let template =
    std::fs::read_to_string(&template_path).map_err(|error| format!("could not read {template_rel}: {error}"))?;
  let fragment_bytes = std::fs::read(fragment).map_err(|error| error.to_string())?;
  let fragment_text = String::from_utf8(fragment_bytes).map_err(|error| format!("fragment is not UTF-8: {error}"))?;
  let body = if math { prerender_math(fragment, vendor, repo_root, node_command)? } else { fragment_text };
  let assembled = splice(&template, &body)?.into_bytes();
  let bad: Vec<(usize, u8)> = assembled
    .iter()
    .enumerate()
    .filter_map(|(offset, byte)| (*byte < 0x20 && *byte != b'\n').then_some((offset, *byte)))
    .collect();
  if !bad.is_empty() {
    let named =
      bad.iter().map(|(offset, byte)| format!("0x{byte:02x} at offset {offset}")).collect::<Vec<_>>().join(", ");
    return Err(format!("assembled {genre} page has {} non-LF control bytes; write aborted: {named}", bad.len()));
  }
  let parent = output.parent().expect("an absolute output path has a parent");
  std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
  std::fs::write(output, assembled).map_err(|error| error.to_string())?;
  Ok(output.to_path_buf())
}

fn prerender_math(
  fragment: &Path,
  vendor: &Path,
  repo_root: &Path,
  node_command: &[OsString],
) -> Result<String, String> {
  ensure_vendored_katex(vendor)?;
  let driver = repo_root.join("src/features/artifacts/prerender_math.js");
  let output = Command::new(&node_command[0])
    .args(&node_command[1..])
    .arg(driver)
    .arg(fragment)
    .arg(vendor)
    .output()
    .map_err(|error| format!("math pre-render driver could not start: {error}"))?;
  if !output.status.success() {
    let code = output.status.code().map_or_else(|| "signal".to_string(), |code| code.to_string());
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    return Err(format!("math pre-render driver failed (exit {code}): {stderr}"));
  }
  String::from_utf8(output.stdout).map_err(|error| format!("math pre-render driver output is not UTF-8: {error}"))
}

fn ensure_vendored_katex(vendor: &Path) -> Result<(), String> {
  if vendor.is_file() {
    return Ok(());
  }
  let parent = vendor.parent().expect("the KaTeX vendor path has a parent");
  std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
  let fetched = Command::new("curl")
    .args(["-fsSL", "--max-time", "60", KATEX_CDN_URL])
    .output()
    .map_err(|error| katex_fetch_error(vendor, parent, &error.to_string()))?;
  if !fetched.status.success() {
    let detail = format!(
      "curl exited {}; {}",
      fetched.status.code().map_or_else(|| "by signal".to_string(), |code| code.to_string()),
      String::from_utf8_lossy(&fetched.stderr).trim()
    );
    return Err(katex_fetch_error(vendor, parent, &detail));
  }
  std::fs::write(vendor, fetched.stdout).map_err(|error| katex_fetch_error(vendor, parent, &error.to_string()))
}

fn katex_fetch_error(vendor: &Path, parent: &Path, detail: &str) -> String {
  format!(
    "vendored KaTeX missing at {} and the CDN fetch failed ({detail}); place it manually:\n  mkdir -p {} && curl -fsSL {KATEX_CDN_URL} -o {}",
    vendor.display(),
    parent.display(),
    vendor.display()
  )
}

fn splice(template: &str, fragment: &str) -> Result<String, String> {
  let body_open = template.find("<body").ok_or_else(|| "genre template has no <body> tag".to_string())?;
  let open_end = template[body_open..]
    .find('>')
    .map(|index| body_open + index + 1)
    .ok_or_else(|| "genre template has an incomplete <body> tag".to_string())?;
  let tail_start = template.find("</body>").ok_or_else(|| "genre template has no </body> tag".to_string())?;
  Ok(format!("{}\n{}\n{}", &template[..open_end], fragment.trim_end_matches('\n'), &template[tail_start..]))
}

fn resolve_path(path: &Path) -> std::io::Result<PathBuf> {
  match std::fs::canonicalize(path) {
    Ok(resolved) => Ok(resolved),
    Err(_) => std::path::absolute(path),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::{Path, PathBuf};

  use super::*;

  const TEST_KATEX: &str = r#"module.exports = {
  renderToString(expression, options) {
    const className = options.displayMode ? "katex-display" : "katex";
    return `<span class="${className}">${expression}</span>`;
  },
};
"#;

  fn node_command(test_name: &str) -> std::io::Result<Option<Vec<OsString>>> {
    let node = executable("node")?;
    let Some(node) = node else {
      println!("skipping {test_name}: node is not installed on host3");
      return Ok(None);
    };
    let timeout = executable("timeout")?.expect("host3 provides timeout for each Node-dependent test");
    Ok(Some(vec![timeout.into_os_string(), OsString::from("20s"), node.into_os_string()]))
  }

  fn executable(name: &str) -> std::io::Result<Option<PathBuf>> {
    for directory in std::env::split_paths(&std::env::var_os("PATH").expect("test runner has PATH")) {
      let path = directory.join(name);
      match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 => return Ok(Some(path)),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
      }
    }
    Ok(None)
  }

  fn vendor(home: &Path) -> PathBuf {
    let path = home.join("vendor/katex/katex.min.js");
    fs::create_dir_all(path.parent().expect("vendor has a parent")).unwrap();
    fs::write(&path, TEST_KATEX).unwrap();
    path
  }

  fn fragment(directory: &Path, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = directory.join("fragment.html");
    fs::write(&path, contents).unwrap();
    path
  }

  fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(4)
      .expect("artifacts crate is under the repository")
      .to_path_buf()
  }

  #[test]
  fn test_wrap_takes_head_and_style_from_the_template_and_the_body_from_the_fragment() {
    let home = tempfile::tempdir().unwrap();
    let Some(node) =
      node_command("test_wrap_takes_head_and_style_from_the_template_and_the_body_from_the_fragment").unwrap()
    else {
      return;
    };
    let fragment = fragment(home.path(), "<div class=\"wrap\"><main><p>body text</p></main></div>");
    let output = home.path().join("page.html");
    wrap_fragment("explain", &fragment, &output, true, &vendor(home.path()), &repo_root(), &node).unwrap();
    let page = fs::read_to_string(output).unwrap();
    assert!(page.starts_with("<!doctype html>"));
    assert!(page.contains("<title>CharlieBot Explain Template</title>"));
    assert!(page.contains("body text"));
    assert!(!page.contains("Why the tint ramp still took 47 minutes"));
    assert!(page.trim_end().ends_with("</html>"));
    assert_eq!(page.matches("<body>").count(), 1);
  }

  #[test]
  fn test_driver_runs_as_a_subprocess() {
    let home = tempfile::tempdir().unwrap();
    let Some(node) = node_command("test_driver_runs_as_a_subprocess").unwrap() else {
      return;
    };
    let fragment = fragment(home.path(), "<p>$x^2$</p>");
    let rendered = prerender_math(&fragment, &vendor(home.path()), &repo_root(), &node).unwrap();
    assert!(rendered.contains("class=\"katex\""));
  }

  #[test]
  fn test_cli_defaults_math_on_for_explain_and_off_for_other_genres() {
    let home = tempfile::tempdir().unwrap();
    let Some(node) = node_command("test_cli_defaults_math_on_for_explain_and_off_for_other_genres").unwrap() else {
      return;
    };
    let explain = WrapArgs {
      fragment: String::new(),
      genre: "explain".to_string(),
      output: String::new(),
      math: false,
      no_math: false,
    };
    let sitrep = WrapArgs {
      fragment: String::new(),
      genre: "sitrep".to_string(),
      output: String::new(),
      math: false,
      no_math: false,
    };
    assert!(explain.math_enabled(&[]));
    assert!(!sitrep.math_enabled(&[]));
    assert!(!explain.math_enabled(&[OsString::from("--no-math")]));
    assert!(explain.math_enabled(&[OsString::from("--math")]));
    assert!(!explain.math_enabled(&[OsString::from("--math"), OsString::from("--no-math")]));

    let source = fragment(home.path(), "<p>$$y = x$$</p>");
    let explain_output = home.path().join("explain.html");
    wrap_fragment(
      "explain",
      &source,
      &explain_output,
      explain.math_enabled(&[]),
      &vendor(home.path()),
      &repo_root(),
      &node,
    )
    .unwrap();
    assert!(fs::read_to_string(explain_output).unwrap().contains("class=\"katex-display\""));

    let sitrep_output = home.path().join("sitrep.html");
    wrap_fragment(
      "sitrep",
      &source,
      &sitrep_output,
      sitrep.math_enabled(&[]),
      &vendor(home.path()),
      &repo_root(),
      &node,
    )
    .unwrap();
    let sitrep_page = fs::read_to_string(sitrep_output).unwrap();
    assert!(!sitrep_page.contains("class=\"katex\""));
    assert!(sitrep_page.contains("$$y = x$$"));
  }

  #[test]
  fn test_wrap_byte_gate_aborts_before_write_and_names_offsets() {
    let home = tempfile::tempdir().unwrap();
    let Some(node) = node_command("test_wrap_byte_gate_aborts_before_write_and_names_offsets").unwrap() else {
      return;
    };
    let damaged =
      "<p>前向中 TABext{out_routed} 属性。</p>\n<p>$$\u{000c}rac{\\partial L}{\\partial TABext{weights}} = 0$$</p>\n"
        .replace("TAB", "\t");
    let source = fragment(home.path(), damaged.as_bytes());
    let output = home.path().join("damaged_page.html");
    let error =
      wrap_fragment("explain", &source, &output, true, &vendor(home.path()), &repo_root(), &node).unwrap_err();
    assert!(error.contains("0x09 at offset"), "{error}");
    assert!(error.contains("0x0c at offset"), "{error}");
    assert!(error.contains("write aborted"), "{error}");
    assert!(!output.exists());
  }

  #[test]
  fn test_wrap_byte_gate_runs_on_the_assembled_bytes() {
    let home = tempfile::tempdir().unwrap();
    let Some(node) = node_command("test_wrap_byte_gate_runs_on_the_assembled_bytes").unwrap() else {
      return;
    };
    let source = fragment(home.path(), "<p>x\ty</p>");
    let output = home.path().join("page.html");
    let error =
      wrap_fragment("explain", &source, &output, true, &vendor(home.path()), &repo_root(), &node).unwrap_err();
    let offset = error
      .split("0x09 at offset ")
      .nth(1)
      .expect("the gate names the tab offset")
      .split_whitespace()
      .next()
      .expect("the offset is a number")
      .parse::<usize>()
      .unwrap();
    assert!(offset > 5000);
    assert!(!output.exists());
  }
}
