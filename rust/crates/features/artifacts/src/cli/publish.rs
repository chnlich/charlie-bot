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
  use std::os::unix::fs::PermissionsExt;
  use std::path::{Path, PathBuf};
  use std::sync::{Arc, Mutex};

  use charliebot_agents::agent_environment::AgentBaseEnvironment;
  use charliebot_agents::backend_pool::BackendPool;
  use charliebot_registry::Registry;
  use serde_yaml::Value;

  use super::*;

  static UMASK_LOCK: Mutex<()> = Mutex::new(());

  struct UmaskReset(libc::mode_t);

  impl UmaskReset {
    fn restrictive() -> UmaskReset {
      UmaskReset(unsafe { libc::umask(0o077) })
    }
  }

  impl Drop for UmaskReset {
    fn drop(&mut self) {
      unsafe {
        libc::umask(self.0);
      }
    }
  }

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
    let path = home.join("artifacts").join("page.html");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "<p>hello</p>").unwrap();
    path
  }

  fn directory_config(dir: Option<PathBuf>, public_base_url: Option<&str>) -> PublishConfig {
    PublishConfig { command: None, dir, public_base_url: public_base_url.map(str::to_string) }
  }

  fn command_config(command: Vec<String>) -> PublishConfig {
    PublishConfig { command: Some(command), dir: None, public_base_url: None }
  }

  fn publish_error_message<T: std::fmt::Debug>(result: Result<T, CliExit>) -> String {
    let error = result.unwrap_err();
    assert_eq!(error.code, 1);
    error.body.get("error").and_then(serde_json::Value::as_str).expect("CLI error has a message").to_string()
  }

  fn write_publish_program(home: &Path, body: &str) -> PathBuf {
    let path = home.join("publish-program");
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
  }

  fn command_for(program: &Path, arguments: &[&str]) -> Vec<String> {
    std::iter::once(program.to_string_lossy().into_owned())
      .chain(arguments.iter().map(|argument| (*argument).to_string()))
      .collect()
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

  #[test]
  fn test_publish_twice_lands_two_token_copies_with_fixed_modes() {
    let _lock = UMASK_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let lane = home_path.join("publish");
    fs::create_dir_all(&lane).unwrap();
    fs::write(lane.join("index.html"), "index").unwrap();
    let artifact = artifact(&home_path);
    let base_urls = [
      "https://pub.example.test/charliebot_pub",
      "https://pub.example.test/charliebot_pub/",
      "https://pub.example.test/charliebot_pub//",
    ];

    let _umask = UmaskReset::restrictive();
    for base_url in base_urls {
      let config = directory_config(Some(lane.clone()), Some(base_url));
      let first = publish_artifact(&artifact, &config, &[]).unwrap();
      let second = publish_artifact(&artifact, &config, &[]).unwrap();
      let tokens = [first, second].map(|url| {
        let suffix = url
          .strip_prefix("https://pub.example.test/charliebot_pub/")
          .expect("the base URL has trailing slashes removed");
        let (token, basename) = suffix.split_once('/').expect("the URL has a token directory");
        assert_eq!(basename, "page.html");
        assert_eq!(token.len(), 22);
        assert!(token.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'));
        let token_dir = lane.join(token);
        let destination = token_dir.join("page.html");
        assert_eq!(fs::read(&destination).unwrap(), b"<p>hello</p>");
        assert_eq!(fs::metadata(&token_dir).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::metadata(&destination).unwrap().permissions().mode() & 0o777, 0o644);
        token.to_string()
      });
      assert_ne!(tokens[0], tokens[1]);
    }
  }

  #[test]
  fn test_publish_without_index_html_refuses_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let lane = home_path.join("publish");
    fs::create_dir_all(&lane).unwrap();
    fs::write(lane.join("index.html"), "index").unwrap();
    let artifact = artifact(&home_path);
    fs::remove_file(lane.join("index.html")).unwrap();
    let config = directory_config(Some(lane.clone()), Some("https://pub.example.test/charliebot_pub"));

    let message = publish_error_message(publish_artifact(&artifact, &config, &[]));

    assert!(message.contains(lane.join("index.html").to_str().unwrap()));
    assert_eq!(fs::read_dir(&lane).unwrap().count(), 0);
  }

  #[test]
  fn test_missing_artifact_raises_naming_the_path() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let lane = home_path.join("publish");
    fs::create_dir_all(&lane).unwrap();
    fs::write(lane.join("index.html"), "index").unwrap();
    let absent = home_path.join("artifacts").join("gone.html");
    let config = directory_config(Some(lane), Some("https://pub.example.test/charliebot_pub"));

    let message = publish_error_message(publish_artifact(&absent, &config, &[]));

    assert!(message.contains(absent.to_str().unwrap()));
  }

  #[test]
  fn test_directory_lane_preflight_messages_stay_word_for_word() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let artifact = artifact(&home_path);
    let no_index = home_path.join("no-index");
    fs::create_dir_all(&no_index).unwrap();
    let missing = home_path.join("missing");
    let cases = [
      (directory_config(None, None), "publish.dir is not configured; the publish lane is unavailable".to_string()),
      (
        directory_config(Some(no_index.clone()), None),
        "publish.public_base_url is not configured; the publish lane is unavailable".to_string(),
      ),
      (
        directory_config(Some(missing.clone()), Some("https://pub.example.test/charliebot_pub")),
        format!("publish directory does not exist: {}", missing.display()),
      ),
      (
        directory_config(Some(no_index.clone()), Some("https://pub.example.test/charliebot_pub")),
        format!(
          "publish directory has no index.html, so its listing would expose every link: {}",
          no_index.join("index.html").display()
        ),
      ),
    ];
    for (config, expected) in cases {
      let message = publish_error_message(publish_artifact(&artifact, &config, &[]));
      assert_eq!(message, expected);
    }

    let absent = home_path.join("artifacts").join("gone.html");
    let lane = home_path.join("publish");
    fs::create_dir_all(&lane).unwrap();
    fs::write(lane.join("index.html"), "index").unwrap();
    let config = directory_config(Some(lane), Some("https://pub.example.test/charliebot_pub"));
    let message = publish_error_message(publish_artifact(&absent, &config, &[]));
    assert_eq!(message, format!("artifact is not an existing regular file: {}", absent.display()));
  }

  #[test]
  fn test_command_lane_returns_the_url_the_program_prints() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let artifact = artifact(&home_path);
    for url in ["https://reader.example/page.html", "http://reader.example/page.html"] {
      let program = write_publish_program(&home_path, &format!("printf '%s\\n' '{url}'\n"));
      let config = command_config(command_for(&program, &[]));

      let result = publish_artifact(&artifact, &config, &[]).unwrap();

      assert_eq!(result, url);
      assert!(!home_path.join("publish").exists());
      fs::remove_file(program).unwrap();
    }
  }

  #[test]
  fn test_command_lane_runs_the_command_on_the_absolute_artifact_path_then_the_lane_options() {
    let current_dir = std::env::current_dir().unwrap();
    let home = tempfile::tempdir_in(&current_dir).unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let artifact = artifact(&home_path);
    let body = format!(
      "[ \"$#\" -eq 5 ] || exit 9\n\
       [ \"$1\" = \"--site\" ] || exit 9\n\
       [ \"$2\" = \"main\" ] || exit 9\n\
       [ \"$3\" = \"{}\" ] || exit 9\n\
       [ \"$4\" = \"--reader\" ] || exit 9\n\
       [ \"$5\" = \"reader@example.com\" ] || exit 9\n\
       printf '%s\\n' 'https://reader.example/page.html'\n",
      artifact.display()
    );
    let program = write_publish_program(&home_path, &body);
    let config = command_config(command_for(&program, &["--site", "main"]));
    let lane_options = [OsString::from("--reader"), OsString::from("reader@example.com")];
    let relative_artifact = artifact.strip_prefix(&current_dir).unwrap();

    let result = publish_artifact(relative_artifact, &config, &lane_options).unwrap();

    assert_eq!(result, "https://reader.example/page.html");
  }

  #[test]
  fn test_command_lane_nonzero_exit_raises_with_the_exit_code_and_stderr() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let artifact = artifact(&home_path);
    let program = write_publish_program(
      &home_path,
      "printf '%s\\n' 'https://reader.example/page.html'\nprintf '%s\\n' 'reader list is locked' >&2\nexit 4\n",
    );
    let config = command_config(command_for(&program, &[]));

    let message = publish_error_message(publish_artifact(&artifact, &config, &[]));

    assert!(message.contains("exited 4"));
    assert!(message.contains("reader list is locked"));
    assert!(!message.contains("https://reader.example/page.html"));
  }

  #[test]
  fn test_command_lane_refuses_stdout_that_is_not_exactly_one_url() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let artifact = artifact(&home_path);
    let outputs = [
      "printf ''\n",
      "printf '%s\\n' 'https://reader.example/page.html' 'https://reader.example/page.html'\n",
      "printf '%s\\n' 'done'\n",
    ];
    for output in outputs {
      let program = write_publish_program(&home_path, output);
      let config = command_config(command_for(&program, &[]));

      let message = publish_error_message(publish_artifact(&artifact, &config, &[]));

      assert!(message.contains("exited 0"));
      fs::remove_file(program).unwrap();
    }
  }

  #[test]
  fn test_command_lane_names_a_program_that_cannot_start() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let artifact = artifact(&home_path);
    let missing = home_path.join("no-such-program");
    let config = command_config(vec![missing.to_string_lossy().into_owned()]);

    let message = publish_error_message(publish_artifact(&artifact, &config, &[]));

    assert!(message.contains(missing.to_str().unwrap()));
  }

  #[test]
  fn test_command_lane_checks_the_artifact_before_running_the_program() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let absent = home_path.join("artifacts").join("gone.html");
    let marker = home_path.join("ran");
    let program = write_publish_program(&home_path, &format!("touch '{}'\n", marker.display()));
    let config = command_config(command_for(&program, &[]));

    let message = publish_error_message(publish_artifact(&absent, &config, &[]));

    assert!(message.contains(absent.to_str().unwrap()));
    assert!(!marker.exists());
  }

  #[test]
  fn test_command_with_a_directory_lane_key_is_a_config_error() {
    for (key, value) in [("dir", "/srv/charliebot_pub"), ("public_base_url", "https://pub.example.com/charliebot_pub")]
    {
      let mut mapping = serde_yaml::Mapping::new();
      mapping.insert(
        Value::String("command".to_string()),
        Value::Sequence(vec![Value::String("publish-program".to_string())]),
      );
      mapping.insert(Value::String(key.to_string()), Value::String(value.to_string()));

      let message = publish_error_message(parse_config(&Value::Mapping(mapping)));

      assert!(message.contains("publish.command"));
      assert!(message.contains(&format!("publish.{key}")));
    }
  }

  #[test]
  fn test_empty_command_list_is_a_config_error() {
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(Value::String("command".to_string()), Value::Sequence(Vec::new()));

    let message = publish_error_message(parse_config(&Value::Mapping(mapping)));

    assert!(message.contains("publish.command is an empty list"));
  }

  #[test]
  fn test_lane_options_on_the_directory_lane_are_refused_before_any_write() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().join("home");
    fs::create_dir_all(&home_path).unwrap();
    let lane = home_path.join("publish");
    fs::create_dir_all(&lane).unwrap();
    fs::write(lane.join("index.html"), "index").unwrap();
    let artifact = artifact(&home_path);
    let config = directory_config(Some(lane.clone()), Some("https://pub.example.test/charliebot_pub"));
    let lane_options = [OsString::from("--reader"), OsString::from("reader@example.com")];

    let message = publish_error_message(publish_artifact(&artifact, &config, &lane_options));

    assert!(message.contains("publish.command"));
    assert_eq!(
      fs::read_dir(&lane).unwrap().map(|entry| entry.unwrap().path()).collect::<Vec<_>>(),
      [lane.join("index.html")]
    );
  }
}
