//! Publish an artifact through the configured program or directory lane.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_agents::cli_host::{CliHost, LazyHost};
use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_core::paths;
use charliebot_core::pyfmt;
use clap::{CommandFactory, FromArgMatches, Parser};
use serde::Deserialize;
use serde_yaml::Value;

const URL_PREFIXES: [&str; 2] = ["http://", "https://"];

#[derive(Parser)]
#[command(
  name = "publish",
  about = "Publish an artifact and print the URL readers outside use",
  after_help = "With publish.command set, arguments after -- go unchanged to its program; the directory lane takes none."
)]
struct PublishCli {
  /// Path of the artifact file to publish
  artifact: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishConfig {
  #[serde(default)]
  command: Option<Vec<String>>,
  #[serde(default)]
  dir: Option<PathBuf>,
  #[serde(default)]
  public_base_url: Option<String>,
}

struct PublishArgs {
  artifact: PathBuf,
  lane_options: Vec<OsString>,
}

/// Parse the publish arguments and print the resulting reader URL.
pub fn main(args: &[OsString], host: &LazyHost) -> Result<(), CliExit> {
  let args = parse_args(args)?;
  let host = host.get().map_err(|error| CliExit::error(format!("{error:#}")))?;
  let url = publish_parsed(&args, host)?;
  println!("{url}");
  Ok(())
}

fn parse_args(args: &[OsString]) -> Result<PublishArgs, CliExit> {
  let separator = args.iter().position(|argument| argument == "--").unwrap_or(args.len());
  let lane_options = if separator == args.len() { Vec::new() } else { args[separator + 1..].to_vec() };
  let command = with_infer_long_args(PublishCli::command());
  let argv = std::iter::once(OsString::from("charliebot publish")).chain(args[..separator].iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = PublishCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  Ok(PublishArgs { artifact: PathBuf::from(cli.artifact), lane_options })
}

fn publish_parsed(args: &PublishArgs, host: &CliHost) -> Result<String, CliExit> {
  let config = parse_config(host.cfg.section::<Value>("publish"))?;
  publish_artifact(&args.artifact, &config, &args.lane_options)
}

fn parse_config(raw: &Value) -> Result<PublishConfig, CliExit> {
  let mut config: PublishConfig =
    serde_yaml::from_value(raw.clone()).map_err(|error| CliExit::error(format!("publish: {error}")))?;
  if let Some(dir) = &config.dir {
    let value = if dir.as_os_str().is_empty() { ".".to_string() } else { dir.to_string_lossy().into_owned() };
    config.dir = Some(PathBuf::from(paths::expanduser(&value)));
  }
  if config.command.as_ref().is_some_and(Vec::is_empty) {
    return Err(CliExit::error(
      "publish.command is an empty list; name the program and its arguments, or leave the key unset",
    ));
  }
  if config.command.is_some() && (config.dir.is_some() || config.public_base_url.is_some()) {
    return Err(CliExit::error(
      "publish.command cannot be set together with publish.dir or publish.public_base_url; a host configures one publish lane",
    ));
  }
  Ok(config)
}

fn publish_artifact(artifact: &Path, config: &PublishConfig, lane_options: &[OsString]) -> Result<String, CliExit> {
  if let Some(command) = &config.command {
    return publish_through_command(artifact, command, lane_options);
  }
  if !lane_options.is_empty() {
    return Err(CliExit::error(
      "the directory lane takes no lane options; set publish.command to publish through an external program",
    ));
  }
  let dir = config
    .dir
    .as_ref()
    .ok_or_else(|| CliExit::error("publish.dir is not configured; the publish lane is unavailable"))?;
  let public_base_url = config
    .public_base_url
    .as_deref()
    .filter(|url| !url.is_empty())
    .ok_or_else(|| CliExit::error("publish.public_base_url is not configured; the publish lane is unavailable"))?;
  if !dir.is_dir() {
    return Err(CliExit::error(format!("publish directory does not exist: {}", dir.display())));
  }
  let index = dir.join("index.html");
  if !index.is_file() {
    return Err(CliExit::error(format!(
      "publish directory has no index.html, so its listing would expose every link: {}",
      index.display()
    )));
  }
  if !artifact.is_file() {
    return Err(CliExit::error(format!("artifact is not an existing regular file: {}", artifact.display())));
  }
  let token = new_token()?;
  let token_dir = dir.join(&token);
  std::fs::create_dir(&token_dir).map_err(|error| CliExit::error(error.to_string()))?;
  std::fs::set_permissions(&token_dir, std::fs::Permissions::from_mode(0o755))
    .map_err(|error| CliExit::error(error.to_string()))?;
  let basename = artifact.file_name().expect("an existing regular file has a basename");
  let destination = token_dir.join(basename);
  std::fs::copy(artifact, &destination).map_err(|error| CliExit::error(error.to_string()))?;
  std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o644))
    .map_err(|error| CliExit::error(error.to_string()))?;
  Ok(format!("{}/{}/{}", public_base_url.trim_end_matches('/'), token, basename.to_string_lossy()))
}

fn new_token() -> Result<String, CliExit> {
  let mut bytes = [0_u8; 16];
  std::fs::File::open("/dev/urandom")
    .and_then(|mut random| random.read_exact(&mut bytes))
    .map_err(|error| CliExit::error(error.to_string()))?;
  Ok(encode_token(&bytes))
}

fn encode_token(bytes: &[u8; 16]) -> String {
  let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  let mut encoded = Vec::with_capacity(22);
  for chunk in bytes.chunks(3) {
    match chunk {
      [first] => {
        encoded.push(alphabet[(first >> 2) as usize]);
        encoded.push(alphabet[((first & 0x03) << 4) as usize]);
      }
      [first, second] => {
        encoded.push(alphabet[(first >> 2) as usize]);
        encoded.push(alphabet[(((first & 0x03) << 4) | (second >> 4)) as usize]);
        encoded.push(alphabet[((second & 0x0f) << 2) as usize]);
      }
      [first, second, third] => {
        encoded.push(alphabet[(first >> 2) as usize]);
        encoded.push(alphabet[(((first & 0x03) << 4) | (second >> 4)) as usize]);
        encoded.push(alphabet[(((second & 0x0f) << 2) | (third >> 6)) as usize]);
        encoded.push(alphabet[(third & 0x3f) as usize]);
      }
      _ => unreachable!("chunks(3) yields groups of one to three bytes"),
    }
  }
  String::from_utf8(encoded).expect("the base64url alphabet is ASCII")
}

fn publish_through_command(artifact: &Path, command: &[String], lane_options: &[OsString]) -> Result<String, CliExit> {
  if !artifact.is_file() {
    return Err(CliExit::error(format!("artifact is not an existing regular file: {}", artifact.display())));
  }
  let artifact_absolute = std::path::absolute(artifact).map_err(|error| CliExit::error(error.to_string()))?;
  let done = Command::new(&command[0]).args(&command[1..]).arg(artifact_absolute).args(lane_options).output().map_err(
    |error| {
      CliExit::error(format!("publish.command could not start {}: {}", command[0], os_error_text(&error, &command[0])))
    },
  )?;
  let stdout = String::from_utf8(done.stdout)
    .map_err(|error| CliExit::error(format!("publish.command stdout is not UTF-8: {error}")))?;
  let stderr = String::from_utf8(done.stderr)
    .map_err(|error| CliExit::error(format!("publish.command stderr is not UTF-8: {error}")))?;
  let stderr = stderr.trim();
  if !done.status.success() {
    use std::os::unix::process::ExitStatusExt;
    let code = done.status.code().unwrap_or_else(|| -done.status.signal().unwrap_or(1));
    return Err(CliExit::error(format!("publish.command exited {code}; stderr: {stderr}")));
  }
  let lines = python_lines(&stdout);
  if lines.len() != 1 || !URL_PREFIXES.iter().any(|prefix| lines[0].starts_with(prefix)) {
    return Err(CliExit::error(format!(
      "publish.command exited 0 without exactly one http(s) URL line on stdout; stderr: {stderr}"
    )));
  }
  Ok(lines[0].to_string())
}

fn os_error_text(error: &io::Error, path: &str) -> String {
  let path = pyfmt::str_repr(path);
  match error.raw_os_error() {
    Some(code) => format!("[Errno {code}] {}: {path}", nix::errno::Errno::from_raw(code).desc()),
    None => format!("{error}: {path}"),
  }
}

fn python_lines(text: &str) -> Vec<&str> {
  let mut lines = Vec::new();
  let mut start = 0;
  let mut chars = text.char_indices().peekable();
  while let Some((index, character)) = chars.next() {
    if matches!(
      character,
      '\n' | '\r' | '\u{000b}' | '\u{000c}' | '\u{001c}'..='\u{001e}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    ) {
      lines.push(&text[start..index]);
      start = index + character.len_utf8();
      if character == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
        let (next_index, next) = chars.next().expect("peeked at LF after CR");
        start = next_index + next.len_utf8();
      }
    }
  }
  if start < text.len() {
    lines.push(&text[start..]);
  }
  lines
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::sync::Arc;

  use charliebot_agents::agent_environment::AgentBaseEnvironment;
  use charliebot_agents::backend_pool::BackendPool;
  use charliebot_registry::Registry;
  use serde_yaml::Value;

  use super::*;

  fn test_host(home: &Path) -> CliHost {
    let mut registry = Registry::new();
    registry.register_package("src.features.artifacts", crate::register).unwrap();
    let registry = Arc::new(registry);
    let cfg = registry.load_config(home).unwrap();
    CliHost {
      cfg,
      registry,
      repo_root: PathBuf::from("/synthetic/checkout"),
      base_env: AgentBaseEnvironment { values: std::env::vars().collect() },
      pool: BackendPool::default(),
    }
  }

  fn setup_directory_lane(home: &Path, lane: &Path) {
    fs::create_dir_all(lane).unwrap();
    fs::write(lane.join("index.html"), "index").unwrap();
    fs::write(
      home.join("config.yaml"),
      format!("publish:\n  dir: \"{}\"\n  public_base_url: https://publish.example///\n", lane.display()),
    )
    .unwrap();
  }

  fn artifact(home: &Path) -> PathBuf {
    let path = home.join("page.html");
    fs::write(&path, "<p>hello</p>").unwrap();
    path
  }

  fn assert_token_url(url: &str, lane: &Path, artifact: &Path) {
    let token = url
      .strip_prefix("https://publish.example/")
      .expect("the configured base URL is used")
      .strip_suffix("/page.html")
      .expect("the artifact basename is in the URL");
    assert_eq!(token.len(), 22);
    assert!(token.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'));
    let token_dir = lane.join(token);
    let destination = token_dir.join("page.html");
    assert_eq!(fs::read(&destination).unwrap(), fs::read(artifact).unwrap());
    assert_eq!(fs::metadata(&token_dir).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(fs::metadata(&destination).unwrap().permissions().mode() & 0o777, 0o644);
  }

  #[test]
  fn token_encoding_matches_urlsafe_base64_without_padding() {
    assert_eq!(encode_token(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]), "AAECAwQFBgcICQoLDA0ODw");
  }

  #[test]
  fn test_publish_prints_the_url_on_stdout_and_exits_zero() {
    let home = tempfile::tempdir().unwrap();
    let lane = home.path().join("publish");
    setup_directory_lane(home.path(), &lane);
    let artifact = artifact(home.path());
    let host = test_host(home.path());
    let args = [OsString::from(artifact.display().to_string())];

    let parsed = parse_args(&args).unwrap();
    let url = publish_parsed(&parsed, &host).unwrap();

    assert_token_url(&url, &lane, &artifact);
  }

  #[test]
  fn test_directory_lane_from_a_yaml_config_without_command_publishes_as_before() {
    let home = tempfile::tempdir().unwrap();
    let lane = home.path().join("lane");
    setup_directory_lane(home.path(), &lane);
    let artifact = artifact(home.path());
    let host = test_host(home.path());
    let raw = host.cfg.section::<Value>("publish");
    assert!(raw.get("command").is_none());
    let args = [OsString::from(artifact.display().to_string())];

    let parsed = parse_args(&args).unwrap();
    let url = publish_parsed(&parsed, &host).unwrap();

    assert_token_url(&url, &lane, &artifact);
  }
}
