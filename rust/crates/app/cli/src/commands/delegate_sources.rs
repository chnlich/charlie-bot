use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_cli_common::exit::CliExit;

use super::task_spec_headings::{is_heading, is_next_heading};

struct Repository {
  root: PathBuf,
  common_dir: PathBuf,
  origin: Option<String>,
}

fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output, CliExit> {
  Command::new("git")
    .arg("-C")
    .arg(dir)
    .args(args)
    .env("LC_ALL", "C")
    .output()
    .map_err(|error| CliExit::error(format!("run git in {}: {error}", dir.display())))
}

fn output_text(output: &[u8]) -> Result<&str, CliExit> {
  std::str::from_utf8(output)
    .map(|text| text.trim_end_matches('\n'))
    .map_err(|error| CliExit::error(format!("decode git output: {error}")))
}

fn repository(dir: &Path) -> Result<Option<Repository>, CliExit> {
  let root = git(dir, &["rev-parse", "--show-toplevel"])?;
  if !root.status.success() {
    let error = output_text(&root.stderr)?;
    if error.starts_with("fatal: not a git repository") {
      return Ok(None);
    }
    return Err(CliExit::error(format!("find git worktree for {}: {error}", dir.display())));
  }
  let root = PathBuf::from(output_text(&root.stdout)?)
    .canonicalize()
    .map_err(|error| CliExit::error(format!("resolve git worktree for {}: {error}", dir.display())))?;
  let common = git(&root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
  if !common.status.success() {
    return Err(CliExit::error(format!("find git common directory: {}", output_text(&common.stderr)?)));
  }
  let common_dir = PathBuf::from(output_text(&common.stdout)?)
    .canonicalize()
    .map_err(|error| CliExit::error(format!("resolve git common directory: {error}")))?;
  let origin = git(&root, &["config", "--get", "remote.origin.url"])?;
  let origin = match origin.status.code() {
    Some(0) => Some(normalize_origin(output_text(&origin.stdout)?)),
    Some(1) => None,
    _ => return Err(CliExit::error(format!("read git origin: {}", output_text(&origin.stderr)?))),
  };
  Ok(Some(Repository { root, common_dir, origin }))
}

fn normalize_origin(origin: &str) -> String {
  let remote = if let Ok(url) = url::Url::parse(origin) {
    if matches!(url.scheme(), "ssh" | "https" | "http") && url.host_str().is_some() {
      let host = url.host_str().expect("host checked above").to_ascii_lowercase();
      let port = url.port().map(|port| format!(":{port}")).unwrap_or_default();
      format!("{host}{port}/{}", url.path().trim_start_matches('/'))
    } else {
      origin.to_string()
    }
  } else if let Some((host, path)) = origin.split_once(':') {
    let host = host.rsplit('@').next().expect("split always yields one part").to_ascii_lowercase();
    format!("{host}/{}", path.trim_start_matches('/'))
  } else {
    origin.to_string()
  };
  let remote = remote.trim_end_matches('/');
  remote.strip_suffix(".git").unwrap_or(remote).to_string()
}

pub(super) fn check_source_files(spec: &str, repo: &Path) -> Result<(), CliExit> {
  let lines: Vec<_> = spec.lines().collect();
  let start = lines.iter().position(|line| is_heading(line, "Source Files")).expect("task spec already validated");
  let mut target = None;
  for line in lines.iter().skip(start + 1).take_while(|line| !is_next_heading(line)) {
    let Some(path) = line.trim().strip_prefix("- /") else { continue };
    let path = PathBuf::from(format!("/{}", path.trim()));
    let resolved =
      path.canonicalize().map_err(|error| CliExit::error(format!("resolve {}: {error}", path.display())))?;
    let directory = if resolved.is_dir() { resolved.as_path() } else { resolved.parent().expect("absolute file path") };
    let Some(source) = repository(directory)? else { continue };
    let target = match &target {
      Some(target) => target,
      None => target.insert(
        repository(repo)?.ok_or_else(|| CliExit::error(format!("--repo is not a git worktree: {}", repo.display())))?,
      ),
    };
    if source.common_dir == target.common_dir || source.origin.is_some() && source.origin == target.origin {
      let relative = resolved.strip_prefix(&source.root).expect("git worktree contains source");
      let relative = if relative.as_os_str().is_empty() { Path::new(".") } else { relative };
      return Err(CliExit::usage(format!(
        "task spec source {} is in repository copy {}; name {} in the task body relative to the worktree",
        path.display(),
        source.root.display(),
        relative.display()
      )));
    }
  }
  Ok(())
}
