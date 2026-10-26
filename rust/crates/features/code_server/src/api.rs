//! The `/api/code-server` route (`src/features/code_server/api.py`): open a folder in code-server, starting it when
//! nothing listens on its port, and the template global that shows the diff page's open button.

use std::collections::HashMap;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Response;
use charliebot_compat::routing::get_only;
use charliebot_compat::{responses, validation};
use charliebot_core::config::{CharlieBotConfig, PathsConfig};
use charliebot_core::paths;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::FeatureContext;
use http::StatusCode;
use minijinja::Value as TemplateValue;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

use crate::config::{self, CodeServerConfig};
use crate::which::which;

const ROUTE: &str = "/api/code-server/open";
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// `CODE_SERVER_START_TIMEOUT`: how long a spawned code-server has to start listening.
const START_TIMEOUT: Duration = Duration::from_secs(5);
const NOT_AVAILABLE_DETAIL: &str = "code-server not available on this host";
/// One client-visible spelling for both 503 raisers.
const START_FAILURE_DETAIL: &str = "failed to start code-server";

/// The `/api/code-server` router, with the feature context as its state.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route(ROUTE, get_only(open)).with_state(Arc::clone(features))
}

pub(crate) fn matches(path: &str) -> bool {
  path == ROUTE
}

/// The template global `code_server_enabled()`: whether the current config resolves an executable.
pub(crate) fn code_server_enabled(features: &FeatureContext) -> Result<TemplateValue, String> {
  let cfg = features.config.get().map_err(|error| error.to_string())?;
  let section = cfg.section::<CodeServerConfig>("code_server");
  let executable = executable(section).map_err(|error| error.to_string())?;
  Ok(TemplateValue::from(executable.is_some()))
}

/// `open_code_server`: the port and the resolved folder, after the code-server on that port is listening.
async fn open(State(features): State<Arc<FeatureContext>>, Query(query): Query<HashMap<String, String>>) -> Response {
  let Some(folder) = query.get("folder") else {
    return validation::response(&[validation::missing(validation::Source::Query, "folder")]);
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("ConfigError", error.to_string()),
  };
  let repo_root = match features.startup.require::<RepoRoot>() {
    Ok(repo_root) => repo_root,
    Err(error) => return responses::internal_error("StateError", error.to_string()),
  };
  open_code_server(&cfg, repo_root.path(), folder).await
}

/// The answer of `open` for `folder`, under the config `cfg` and the repository `repo_root`.
pub(crate) async fn open_code_server(cfg: &CharlieBotConfig, repo_root: &Path, folder: &str) -> Response {
  let section = cfg.section::<CodeServerConfig>("code_server").clone();
  let paths = cfg.paths.clone();
  let repo_root = repo_root.to_path_buf();
  let folder = folder.to_owned();
  let setup = tokio::task::spawn_blocking(move || prepare_open(&section, &paths, &repo_root, &folder)).await;
  let setup = match setup {
    Ok(Ok(setup)) => setup,
    Ok(Err(response)) => return response,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };

  if !is_listening(setup.port).await {
    let mut child = match spawn(&setup.binary, &setup.config_path) {
      Ok(child) => child,
      Err(error) => {
        tracing::error!(error = %error, "code_server_start_failed");
        return http_error(StatusCode::SERVICE_UNAVAILABLE, START_FAILURE_DETAIL);
      }
    };
    if let Err(response) = wait_for_listener(&mut child, setup.port).await {
      return response;
    }
    if !is_listening(setup.port).await {
      return http_error(StatusCode::SERVICE_UNAVAILABLE, START_FAILURE_DETAIL);
    }
  }

  responses::json(StatusCode::OK, &json!({"port": setup.port, "folder": paths::display(&setup.folder_path)}))
}

struct OpenSetup {
  binary: String,
  folder_path: PathBuf,
  config_path: PathBuf,
  port: u16,
}

/// `_resolve_code_server_executable`, `_resolve_folder_under_allowed_root` and config-file reads all run in the
/// FastAPI sync endpoint's thread pool (`src/features/code_server/api.py:76-90`).
fn prepare_open(
  section: &CodeServerConfig,
  paths: &PathsConfig,
  repo_root: &Path,
  folder: &str,
) -> Result<OpenSetup, Response> {
  let Some(binary) = executable(section).map_err(|error| responses::internal_error("OSError", error.to_string()))?
  else {
    return Err(http_error(StatusCode::NOT_FOUND, NOT_AVAILABLE_DETAIL));
  };
  let folder_path = resolve_folder(folder, paths)?;
  let config_path = config::config_path(section, repo_root);
  let port = match config::listen_port(&config_path) {
    Ok(port) => port,
    Err(detail) => {
      tracing::error!(error = %detail, "code_server_config_invalid");
      return Err(http_error(StatusCode::INTERNAL_SERVER_ERROR, &detail));
    }
  };
  // `_is_listening` takes a socket port: a value beyond 0-65535 fails inside the socket call, unhandled.
  let Ok(port) = u16::try_from(port) else {
    return Err(responses::internal_error("OverflowError", "port must be 0-65535.".to_string()));
  };
  Ok(OpenSetup { binary, folder_path, config_path, port })
}

/// The poll loop after a spawn: every 0.2 s for up to 5 s, stopping early when the process exits.
async fn wait_for_listener(child: &mut Child, port: u16) -> Result<(), Response> {
  let deadline = Instant::now() + START_TIMEOUT;
  while Instant::now() < deadline {
    if is_listening(port).await {
      break;
    }
    match child.try_wait() {
      Ok(Some(status)) => {
        tracing::warn!(returncode = return_code(status), "code_server_exited_before_listening");
        break;
      }
      Ok(None) => {}
      Err(error) => return Err(responses::internal_error("OSError", error.to_string())),
    }
    tokio::time::sleep(POLL_INTERVAL).await;
  }
  Ok(())
}

/// `Popen.returncode`: the exit code, or the negated signal number for a process that was signalled.
fn return_code(status: ExitStatus) -> i32 {
  status.code().or(status.signal().map(|signal| -signal)).expect("an exit status has a code or a signal")
}

/// `_is_listening`: a connect to `127.0.0.1:<port>` within 0.2 s succeeds.
async fn is_listening(port: u16) -> bool {
  matches!(tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(("127.0.0.1", port))).await, Ok(Ok(_)))
}

/// `_start_code_server`: `<binary> --config <path>` in a new session, with stdin, stdout and stderr closed. The child
/// outlives the request.
fn spawn(binary: &str, config_path: &Path) -> io::Result<Child> {
  let mut command = Command::new(binary);
  command.arg("--config").arg(config_path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
  // SAFETY: `setsid` is async-signal-safe, and the closure runs between fork and exec with nothing else to touch.
  unsafe {
    command.as_std_mut().pre_exec(|| {
      if libc::setsid() == -1 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    });
  }
  command.spawn()
}

/// `_resolve_code_server_executable`: `code_server.bin` (with `~` expanded) when set, else `code-server`.
pub(crate) fn executable(section: &CodeServerConfig) -> anyhow::Result<Option<String>> {
  let search_path = std::env::var_os("PATH").map(|path| path.to_string_lossy().into_owned());
  match section.bin.as_deref().filter(|bin| !bin.is_empty()) {
    Some(bin) => which(&paths::expanduser(&config::pathlib_text(bin)), search_path.as_deref()),
    None => which("code-server", search_path.as_deref()),
  }
}

/// `_resolve_folder_under_allowed_root`: the folder, resolved, when it is a directory under a workspace directory or
/// the worktree directory.
fn resolve_folder(folder: &str, paths: &PathsConfig) -> Result<PathBuf, Response> {
  let folder_path = paths::realpath(&PathBuf::from(paths::expanduser(&config::pathlib_text(folder))));
  if !folder_path.is_dir() {
    return Err(http_error(StatusCode::BAD_REQUEST, &format!("Not a directory: {folder}")));
  }
  let mut roots: Vec<PathBuf> =
    paths.workspace_dirs.iter().map(|dir| paths::realpath(&PathBuf::from(paths::expanduser(dir)))).collect();
  roots.push(paths::realpath(&PathBuf::from(paths::expanduser(&paths.worktree_dir))));
  if !roots.iter().any(|root| folder_path.starts_with(root)) {
    return Err(http_error(
      StatusCode::BAD_REQUEST,
      "folder must be under configured paths.workspace_dirs or paths.worktree_dir",
    ));
  }
  Ok(folder_path)
}

/// `HTTPException(status, detail=...)`: `{"detail": ...}` at `status`.
fn http_error(status: StatusCode, detail: &str) -> Response {
  responses::json(status, &json!({"detail": detail}))
}
