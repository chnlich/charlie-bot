//! The open route's answers: no executable, a folder outside the roots, an invalid `bind-addr`, a stub executable that
//! listens on the configured port, and one that exits before it listens.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use axum::body::to_bytes;
use axum::http::StatusCode;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::Registry;
use charliebot_shell_harness::write_executable;
use serde_json::{Value, json};

use crate::api::{executable, open_code_server};
use crate::config::CodeServerConfig;
use crate::register;

/// Loads a home whose config holds `code_server` and names `workspace` as the workspace directory and `home/worktrees`
/// as the worktree directory.
fn load_config(home: &Path, code_server: Value, workspace: &Path) -> CharlieBotConfig {
  let mut registry = Registry::new();
  registry.register_package("src.features.code_server", register).expect("the package registers");
  fs::create_dir_all(home).expect("the home exists");
  let config = json!({
    "code_server": code_server,
    "paths": {
      "workspace_dirs": [workspace.to_str().expect("UTF-8")],
      "worktree_dir": home.join("worktrees").to_str().expect("UTF-8"),
    },
  });
  fs::write(home.join("config.yaml"), config.to_string()).expect("the config writes");
  registry.load_config(home).expect("the config loads")
}

/// A `bind-addr` config file under the repository, naming `bind_addr`.
fn write_code_server_config(repo_root: &Path, bind_addr: &str) {
  fs::create_dir_all(repo_root.join("configs")).expect("the configs directory exists");
  fs::write(repo_root.join("configs/code-server.yaml"), format!("bind-addr: {bind_addr}\n"))
    .expect("the code-server config writes");
}

/// A stub that listens on `port` and exits once the test process does, so it outlives no test run. It is a `sh`
/// wrapper whose `exec` runs the `listening_stub` example, so the example keeps the wrapper's process and parent.
fn listening_stub(path: &Path, port: u16) {
  let binary = built_example("listening_stub");
  write_executable(path, format!("#!/bin/sh\nexec '{}' {port}\n", binary.display()));
}

/// The example `name`, which `cargo test` builds into the `examples` directory beside the test binary's profile.
fn built_example(name: &str) -> PathBuf {
  let test_binary = std::env::current_exe().expect("the test binary path reads");
  let profile = test_binary.parent().and_then(Path::parent).expect("the test binary sits in a profile directory");
  let binary = profile.join("examples").join(name);
  assert!(binary.is_file(), "cargo test builds the example at {}", binary.display());
  binary
}

/// A port nothing listens on at the moment of the call.
fn free_port() -> u16 {
  TcpListener::bind("127.0.0.1:0").expect("a loopback port binds").local_addr().expect("the address reads").port()
}

async fn body_of(response: axum::response::Response) -> (StatusCode, Value) {
  let status = response.status();
  let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
  (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The folder under the workspace the open route is asked for.
fn project_under(workspace: &Path) -> PathBuf {
  let project = workspace.join("project");
  fs::create_dir_all(&project).expect("the project directory exists");
  project
}

#[tokio::test]
async fn a_missing_executable_answers_404() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let workspace = dir.path().join("workspace");
  let cfg = load_config(
    &dir.path().join("home"),
    json!({"bin": dir.path().join("no-such-code-server").to_str().unwrap()}),
    &workspace,
  );
  let folder = project_under(&workspace);

  let (status, body) = body_of(open_code_server(&cfg, dir.path(), folder.to_str().unwrap()).await).await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  assert_eq!(body, json!({"detail": "code-server not available on this host"}));
}

#[tokio::test]
async fn a_folder_outside_the_workspace_and_the_worktrees_answers_400() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let bin = dir.path().join("code-server");
  write_executable(&bin, "#!/bin/sh\nexit 0\n");
  let workspace = dir.path().join("workspace");
  let cfg = load_config(&dir.path().join("home"), json!({"bin": bin.to_str().unwrap()}), &workspace);
  let outside = dir.path().join("outside");
  fs::create_dir(&outside).expect("the folder exists");

  let (status, body) = body_of(open_code_server(&cfg, dir.path(), outside.to_str().unwrap()).await).await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert_eq!(body, json!({"detail": "folder must be under configured paths.workspace_dirs or paths.worktree_dir"}));
}

#[tokio::test]
async fn a_bind_addr_without_a_port_answers_500_with_the_config_error() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let bin = dir.path().join("code-server");
  write_executable(&bin, "#!/bin/sh\nexit 0\n");
  let workspace = dir.path().join("workspace");
  let cfg = load_config(&dir.path().join("home"), json!({"bin": bin.to_str().unwrap()}), &workspace);
  write_code_server_config(dir.path(), "127.0.0.1");
  let folder = project_under(&workspace);

  let (status, body) = body_of(open_code_server(&cfg, dir.path(), folder.to_str().unwrap()).await).await;
  assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
  let config_file = dir.path().join("configs/code-server.yaml");
  assert_eq!(body, json!({"detail": format!("code-server config must define bind-addr: {}", config_file.display())}));
}

#[tokio::test]
async fn a_stub_that_listens_on_the_configured_port_answers_200_with_the_port_and_the_folder() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let port = free_port();
  let bin = dir.path().join("code-server");
  listening_stub(&bin, port);
  let workspace = dir.path().join("workspace");
  let cfg = load_config(&dir.path().join("home"), json!({"bin": bin.to_str().unwrap()}), &workspace);
  write_code_server_config(dir.path(), &format!("127.0.0.1:{port}"));
  let folder = project_under(&workspace);

  let (status, body) = body_of(open_code_server(&cfg, dir.path(), folder.to_str().unwrap()).await).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body, json!({"port": port, "folder": fs::canonicalize(&folder).unwrap().to_str().unwrap()}));
}

#[tokio::test]
async fn a_stub_that_exits_before_listening_answers_503() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let port = free_port();
  let bin = dir.path().join("code-server");
  write_executable(&bin, "#!/bin/sh\nexit 3\n");
  let workspace = dir.path().join("workspace");
  let cfg = load_config(&dir.path().join("home"), json!({"bin": bin.to_str().unwrap()}), &workspace);
  write_code_server_config(dir.path(), &format!("127.0.0.1:{port}"));
  let folder = project_under(&workspace);

  let (status, body) = body_of(open_code_server(&cfg, dir.path(), folder.to_str().unwrap()).await).await;
  assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
  assert_eq!(body, json!({"detail": "failed to start code-server"}));
}

#[test]
fn the_bin_setting_names_the_executable_and_an_unset_one_searches_the_path() {
  let dir = tempfile::tempdir().expect("a temp dir");
  let bin = dir.path().join("code-server");
  write_executable(&bin, "#!/bin/sh\nexit 0\n");
  let cfg = load_config(&dir.path().join("home"), json!({"bin": bin.to_str().unwrap()}), dir.path());
  let section = cfg.section::<CodeServerConfig>("code_server");
  assert_eq!(executable(section).unwrap(), Some(bin.to_str().unwrap().to_string()));

  let missing = load_config(
    &dir.path().join("other-home"),
    json!({"bin": dir.path().join("absent").to_str().unwrap()}),
    dir.path(),
  );
  assert_eq!(executable(missing.section::<CodeServerConfig>("code_server")).unwrap(), None);
}
