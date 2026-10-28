use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use charliebot_cli_common::args::with_infer_long_args;
use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::http;
use charliebot_core::config;
use charliebot_core::credentials;
use charliebot_core::home;
use charliebot_core::paths;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};

const GRAPHQL_ENDPOINT: &str = "https://api.linear.app/graphql";

#[derive(Parser)]
#[command(name = "linear", subcommand_required = true, arg_required_else_help = true)]
struct LinearCli {
  #[command(subcommand)]
  command: LinearCommand,
}

#[derive(Subcommand)]
enum LinearCommand {
  Graphql(GraphqlArgs),
}

#[derive(Args)]
struct GraphqlArgs {
  #[arg(long, value_name = "PATH")]
  file: Option<PathBuf>,
}

/// Run the `linear` subcommand.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let command = with_infer_long_args(LinearCli::command());
  let argv = std::iter::once(OsString::from("charliebot linear")).chain(args.iter().cloned());
  let matches = match command.try_get_matches_from(argv) {
    Ok(matches) => matches,
    Err(error) => error.exit(),
  };
  let cli = LinearCli::from_arg_matches(&matches).expect("try_get_matches_from validated the arguments");
  match cli.command {
    LinearCommand::Graphql(args) => {
      let request_body = read_request(args.file.as_deref())?;
      let home =
        home::charliebot_home_dir().map_err(|_| credential_failure(Path::new("$CHARLIEBOT_HOME/credentials.yaml")))?;
      let response = execute_graphql(&request_body, GRAPHQL_ENDPOINT, &home)?;
      std::io::stdout().write_all(&response).map_err(|_| CliExit::error("could not write the Linear response"))?;
      Ok(())
    }
  }
}

fn read_request(file: Option<&Path>) -> Result<Vec<u8>, CliExit> {
  let mut body = Vec::new();
  match file {
    Some(path) => std::fs::File::open(path)
      .and_then(|mut input| input.read_to_end(&mut body))
      .map_err(|_| CliExit::usage("could not read the request body"))?,
    None => std::io::stdin().read_to_end(&mut body).map_err(|_| CliExit::usage("could not read the request body"))?,
  };
  Ok(body)
}

fn validate_request_body(body: &[u8]) -> Result<(), CliExit> {
  let value: Value = serde_json::from_slice(body).map_err(|_| invalid_request_body())?;
  let Some(object) = value.as_object() else {
    return Err(invalid_request_body());
  };
  if !object.get("query").is_some_and(Value::is_string)
    || object.get("variables").is_some_and(|variables| !variables.is_object())
  {
    return Err(invalid_request_body());
  }
  Ok(())
}

fn invalid_request_body() -> CliExit {
  CliExit::usage("request body must be a JSON object with a string query and an optional variables object")
}

fn execute_graphql(body: &[u8], endpoint: &str, home: &Path) -> Result<Vec<u8>, CliExit> {
  validate_request_body(body)?;
  let api_key = read_api_key(home)?;
  http::ensure_crypto_provider();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|_| response_failure(None, "request failed", &api_key))?;
  runtime.block_on(async {
    let client = reqwest::Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .map_err(|_| response_failure(None, "request failed", &api_key))?;
    let response = client
      .post(endpoint)
      .header(reqwest::header::CONTENT_TYPE, "application/json")
      .header(reqwest::header::AUTHORIZATION, api_key.as_str())
      .body(body.to_vec())
      .send()
      .await
      .map_err(|_| response_failure(None, "request failed", &api_key))?;
    let status = response.status().as_u16();
    let response_body =
      response.bytes().await.map_err(|_| response_failure(Some(status), "response body read failed", &api_key))?;
    if status != 200 {
      return Err(response_failure(Some(status), &String::from_utf8_lossy(&response_body), &api_key));
    }
    let value: Value = serde_json::from_slice(&response_body)
      .map_err(|_| response_failure(Some(status), &String::from_utf8_lossy(&response_body), &api_key))?;
    if value.as_object().is_some_and(|object| object.contains_key("errors")) {
      return Err(response_failure(Some(status), &String::from_utf8_lossy(&response_body), &api_key));
    }
    Ok(response_body.to_vec())
  })
}

fn read_api_key(home: &Path) -> Result<String, CliExit> {
  let path = home.join(config::CREDENTIALS_FILENAME);
  let loaded = credentials::load_credentials(home).map_err(|_| credential_failure(&path))?;
  loaded.require("linear", "api_key").map_err(|_| credential_failure(&path))
}

fn credential_failure(path: &Path) -> CliExit {
  CliExit::contract(
    json!({"error": format!("credentials.linear.api_key is unavailable in {}", paths::display(path))}),
    3,
  )
}

fn response_failure(status: Option<u16>, response: &str, api_key: &str) -> CliExit {
  let safe_response = if api_key.is_empty() { response.to_string() } else { response.replace(api_key, "[redacted]") };
  CliExit::contract(json!({"status": status, "response": safe_response}), 1)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::{Read, Write};
  use std::net::{TcpListener, TcpStream};
  use std::thread;
  use std::time::Duration;

  const API_KEY: &str = "synthetic-linear-key";
  const REQUEST_BODY: &[u8] = br#"{"query":"query { viewer { id } }","variables":{"id":"synthetic-id"}}"#;

  fn credentials_home(key: Option<&str>) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    if let Some(key) = key {
      std::fs::write(home.path().join(config::CREDENTIALS_FILENAME), format!("linear:\n  api_key: {key}\n")).unwrap();
    }
    home
  }

  fn start_stub(status: u16, response_body: Vec<u8>) -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let request = read_http_request(&mut stream);
      sender.send(request).unwrap();
      let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Response",
      };
      let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len()
      );
      stream.write_all(response.as_bytes()).unwrap();
      stream.write_all(&response_body).unwrap();
    });
    (format!("http://{address}/graphql"), receiver)
  }

  fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
      let read = stream.read(&mut buffer).unwrap();
      if read == 0 {
        return request;
      }
      request.extend_from_slice(&buffer[..read]);
      let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n").map(|index| index + 4) else {
        continue;
      };
      let header_text = String::from_utf8_lossy(&request[..header_end]);
      let content_length = header_text
        .lines()
        .find_map(|line| {
          let (name, value) = line.split_once(':')?;
          name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
      if request.len() >= header_end + content_length {
        return request;
      }
    }
  }

  fn error_text(error: &CliExit) -> String {
    serde_json::to_string(&error.body).unwrap()
  }

  fn assert_no_key(text: &str) {
    assert!(!text.contains(API_KEY), "an output contains a synthetic credential");
  }

  #[test]
  fn graphql_posts_the_key_without_bearer_and_returns_the_response_unchanged() {
    let home = credentials_home(Some(API_KEY));
    let response_body = br#"{"data":{"viewer":{"id":"synthetic-id"}}}"#.to_vec();
    let (endpoint, request_receiver) = start_stub(200, response_body.clone());

    let response = execute_graphql(REQUEST_BODY, &endpoint, home.path()).unwrap();
    let request = request_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    let request_text = String::from_utf8(request).unwrap();
    let lowered = request_text.to_ascii_lowercase();

    assert_eq!(response, response_body);
    assert!(lowered.contains("authorization: synthetic-linear-key"));
    assert!(!lowered.contains("authorization: bearer"));
    assert!(lowered.contains("content-type: application/json"));
    assert!(request_text.ends_with(std::str::from_utf8(REQUEST_BODY).unwrap()));
    assert_no_key(std::str::from_utf8(&response).unwrap());
  }

  #[test]
  fn graphql_errors_include_status_and_redact_the_key() {
    let home = credentials_home(Some(API_KEY));
    for (status, response_body) in [
      (200, br#"{"errors":[{"message":"failed"}]}"#.to_vec()),
      (400, API_KEY.as_bytes().to_vec()),
      (200, b"not-json".to_vec()),
    ] {
      let (endpoint, request_receiver) = start_stub(status, response_body);
      let error = execute_graphql(REQUEST_BODY, &endpoint, home.path()).unwrap_err();
      request_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
      let text = error_text(&error);
      assert_eq!(error.code, 1);
      assert_no_key(&text);
      assert!(text.contains(&format!("\"status\":{status}")));
    }
  }

  #[test]
  fn graphql_network_and_credential_failures_do_not_include_the_key() {
    let home = credentials_home(Some(API_KEY));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let error = execute_graphql(REQUEST_BODY, &format!("http://127.0.0.1:{port}/graphql"), home.path()).unwrap_err();
    assert_eq!(error.code, 1);
    assert_no_key(&error_text(&error));

    let missing_home = credentials_home(None);
    let error = execute_graphql(REQUEST_BODY, "http://127.0.0.1:1/graphql", missing_home.path()).unwrap_err();
    assert_eq!(error.code, 3);
    assert_no_key(&error_text(&error));
    assert!(error_text(&error).contains("credentials.linear.api_key"));
    assert!(error_text(&error).contains("credentials.yaml"));
  }

  #[test]
  fn invalid_request_bodies_exit_two_without_echoing_input() {
    let home = credentials_home(None);
    let invalid = format!("{{not-json {API_KEY}");
    let error = execute_graphql(invalid.as_bytes(), "http://127.0.0.1:1/graphql", home.path()).unwrap_err();

    assert_eq!(error.code, 2);
    assert_no_key(&error_text(&error));
  }

  #[test]
  fn request_files_are_read_and_variables_must_be_an_object() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("request.json");
    std::fs::write(&path, REQUEST_BODY).unwrap();
    assert_eq!(read_request(Some(&path)).unwrap(), REQUEST_BODY);
    assert!(validate_request_body(br#"{"query":"query { viewer { id } }","variables":[]}"#).is_err());
  }
}
