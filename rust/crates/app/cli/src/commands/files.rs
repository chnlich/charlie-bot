use std::ffi::OsString;
use std::time::Duration;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::base_url;
use charliebot_cli_common::exit::{self, CliExit};
use charliebot_cli_common::headers;
use charliebot_cli_common::http;
use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::home;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(name = "files", subcommand_required = true, arg_required_else_help = true)]
struct FilesCli {
  #[command(subcommand)]
  command: FilesCommand,
}

#[derive(Subcommand)]
enum FilesCommand {
  Probe(ProbeArgs),
}

#[derive(Args)]
struct ProbeArgs {
  link: String,
}

/// Run the `files` subcommand.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(FilesCli::command());
  let argv = std::iter::once(OsString::from("charliebot files")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = FilesCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    FilesCommand::Probe(args) => {
      println!("{}", probe(&args.link)?);
      Ok(())
    }
  }
}

fn probe(link: &str) -> Result<String, CliExit> {
  let path = path_and_query(link)?;
  let home =
    home::charliebot_home_dir().map_err(|error| probe_failure(None, &path, Some(exit::source_chain(&error))))?;
  let port = base_url::server_port(&home).map_err(|error| {
    let cause = error.body["error"].as_str().expect("base URL errors carry an error message").to_string();
    probe_failure(None, &path, Some(cause))
  })?;
  let auth_headers = headers::internal_api_auth_headers()
    .map_err(|_| probe_failure(None, &path, Some(format!("$CHARLIEBOT_HOME/{CREDENTIALS_FILENAME}"))))?;
  let url = format!("http://localhost:{port}{path}");

  http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|error| probe_failure(None, &path, Some(exit::source_chain(&error))))?;
  let status = runtime.block_on(async {
    let client = reqwest::Client::builder()
      .no_proxy()
      .redirect(reqwest::redirect::Policy::none())
      .timeout(Duration::from_secs(10))
      .build()
      .map_err(|error| probe_failure(None, &path, Some(exit::source_chain(&error))))?;
    let mut request = client.get(&url);
    for (name, value) in auth_headers {
      request = request.header(name, value);
    }
    let response =
      request.send().await.map_err(|error| probe_failure(None, &path, Some(exit::source_chain(&error))))?;
    Ok::<u16, CliExit>(response.status().as_u16())
  })?;

  if status == 200 { Ok(format!("200 {path}")) } else { Err(probe_failure(Some(status), &path, None)) }
}

fn path_and_query(link: &str) -> Result<String, CliExit> {
  let parsed = if link.starts_with('/') {
    url::Url::parse(&format!("http://localhost{link}"))
  } else {
    url::Url::parse(link).and_then(|url| {
      if url.host_str().is_some() && !url.cannot_be_a_base() { Ok(url) } else { Err(url::ParseError::EmptyHost) }
    })
  }
  .map_err(|_| CliExit::usage("link must be a full URL or a server path starting with /"))?;

  let mut path = parsed.path().to_string();
  if let Some(query) = parsed.query() {
    path.push('?');
    path.push_str(query);
  }
  Ok(path)
}

fn probe_failure(status: Option<u16>, path: &str, cause: Option<String>) -> CliExit {
  let mut body = json!({"status": status, "path": path});
  if status.is_none() {
    body["error"] = json!(cause.expect("failures without an HTTP status carry a cause"));
  }
  CliExit::contract(body, 1)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_relative_link_is_an_argument_error() {
    assert_eq!(path_and_query("relative/file").unwrap_err().code, 2);
  }

  #[test]
  fn a_server_path_keeps_escapes_and_encodes_browser_path_characters() {
    assert_eq!(path_and_query("/files/a b/雪?encoded=%2F#fragment").unwrap(), "/files/a%20b/%E9%9B%AA?encoded=%2F");
  }

  #[test]
  fn a_full_url_uses_only_its_encoded_path_and_query() {
    assert_eq!(
      path_and_query("https://unrelated.invalid/a b?value=雪%2f#fragment").unwrap(),
      "/a%20b?value=%E9%9B%AA%2f"
    );
  }
}
