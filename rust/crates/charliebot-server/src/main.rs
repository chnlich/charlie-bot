//! `charliebot-server`: the CharlieBot HTTP server (`server.py`).
//!
//! Startup follows `server.main` then the lifespan: load and check the config, take the home writer
//! fence (another holder exits 3 after logging `server_startup_refused_home_writer_fence`), capture
//! the build identity, initialize the home, log `charliebot_home_ready` and `server_ready`, then bind
//! the configured host and port. SIGTERM or SIGINT stops accepting, lets in-flight requests finish for
//! up to five seconds, releases the fence, logs `charliebot_shutdown`, then raises the signal again under
//! its default disposition, so the process ends killed by that signal as uvicorn's does.
//!
//! `CHARLIEBOT_REPO_ROOT` names the charlie-bot checkout whose `web/`, page packages and `configs/`
//! the server serves; the Python server derives the same root from its own file location.

mod app;
mod auth;
mod buildinfo;
mod cookies;
mod git;
mod gzip;
mod home_init;
mod mimetypes;
mod mimetypes_table;
mod request_log;
mod request_path;
mod responses;
mod routes;
mod state;
mod static_files;
mod templating;

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use charliebot_core::fence::{self, FenceError};
use charliebot_core::{config, home, logging, paths};
use nix::sys::signal::{SigHandler, Signal};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::app::ConnectionInfo;
use crate::buildinfo::BuildInfo;
use crate::mimetypes::MimeTypes;
use crate::routes::pages::VoiceSettings;
use crate::state::AppState;
use crate::static_files::StaticMount;
use crate::templating::Templating;

/// The environment variable naming the charlie-bot checkout.
const REPO_ROOT_ENV: &str = "CHARLIEBOT_REPO_ROOT";
/// `timeouts.SERVER_GRACEFUL_SHUTDOWN_TIMEOUT`: how long in-flight requests get after a stop signal.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// A config or environment error (Python's `main` raising).
const EXIT_CONFIG: u8 = 1;
/// A lifespan startup failure (uvicorn's "Application startup failed").
const EXIT_STARTUP: u8 = 3;

fn main() -> ExitCode {
  logging::init();
  let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("the tokio runtime builds");
  runtime.block_on(run())
}

async fn run() -> ExitCode {
  let mut stop = match StopSignal::install() {
    Ok(stop) => stop,
    Err(error) => {
      eprintln!("charliebot-server: installing the signal handlers failed: {error}");
      return ExitCode::from(EXIT_STARTUP);
    }
  };
  let boot = Instant::now();
  let (repo_root, state_parts) = match load_inputs() {
    Ok(inputs) => inputs,
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  let cfg = state_parts.config.get().expect("load_inputs loaded the config");

  let mut writer_fence = match fence::acquire_home_writer_fence(&cfg.charliebot_home, "server startup") {
    Ok(writer_fence) => writer_fence,
    Err(error @ FenceError::Active { .. }) => {
      tracing::error!(
        home = %paths::display(&cfg.charliebot_home),
        error = %error,
        "server_startup_refused_home_writer_fence"
      );
      return ExitCode::from(EXIT_STARTUP);
    }
    Err(error) => {
      eprintln!("charliebot-server: taking the home writer fence failed: {error}");
      return ExitCode::from(EXIT_STARTUP);
    }
  };

  let build_info = BuildInfo::capture(&repo_root);
  if let Err(error) = home_init::init_charliebot_home(&cfg, &repo_root) {
    writer_fence.release();
    eprintln!("charliebot-server: initializing {} failed: {error}", paths::display(&cfg.charliebot_home));
    return ExitCode::from(EXIT_STARTUP);
  }
  tracing::info!(path = %paths::display(&cfg.charliebot_home), "charliebot_home_ready");
  let state = Arc::new(AppState {
    templating: Templating::new(&repo_root),
    static_mounts: StaticMount::python_app_mounts(&repo_root),
    mime_types: MimeTypes::load(),
    config: state_parts.config,
    credentials: state_parts.credentials,
    build_info,
  });
  tracing::info!(ready_in_ms = round_ms(boot.elapsed()), "server_ready");

  let listener = match bind(&cfg.server.host, cfg.server.port).await {
    Ok(listener) => listener,
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      let shutdown_started = Instant::now();
      writer_fence.release();
      tracing::info!(shutdown_ms = round_ms(shutdown_started.elapsed()), "charliebot_shutdown");
      return ExitCode::from(EXIT_CONFIG);
    }
  };

  let service = axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<ConnectionInfo>(
    app::build(state),
  );
  let (stop_sender, stop_receiver) = tokio::sync::oneshot::channel::<()>();
  let server = tokio::spawn(async move {
    axum::serve(listener, service)
      .with_graceful_shutdown(async move {
        // A dropped sender also stops the server.
        let _ = stop_receiver.await;
      })
      .await
  });
  let received = stop.wait().await;
  // The receiver lives until the server task ends; a send to an ended server has nothing to stop.
  let _ = stop_sender.send(());
  match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, server).await {
    Ok(Ok(Ok(()))) => {}
    Ok(Ok(Err(error))) => tracing::error!(error = %error, "server_failed"),
    Ok(Err(join_error)) => tracing::error!(error = %join_error, "server_failed"),
    Err(_) => tracing::warn!(timeout_s = GRACEFUL_SHUTDOWN_TIMEOUT.as_secs(), "graceful_shutdown_timeout_exceeded"),
  }
  let shutdown_started = Instant::now();
  writer_fence.release();
  tracing::info!(shutdown_ms = round_ms(shutdown_started.elapsed()), "charliebot_shutdown");
  exit_by_signal(received)
}

/// uvicorn's `capture_signals` exit: restore the default disposition and raise the stop signal again.
fn exit_by_signal(received: Signal) -> ExitCode {
  // SAFETY: SIG_DFL installs no handler code, so no handler can run concurrently with the change.
  unsafe { nix::sys::signal::signal(received, SigHandler::SigDfl) }.expect("restoring a default disposition succeeds");
  nix::sys::signal::raise(received).expect("raising a signal at this process succeeds");
  unreachable!("the default {received} disposition terminates the process")
}

/// The two hot-reloaded files, loaded once at startup.
struct StateParts {
  config: charliebot_core::hot_reload::HotReload<config::CharlieBotConfig>,
  credentials: charliebot_core::hot_reload::HotReload<charliebot_core::credentials::Credentials>,
}

/// The repository root and the home's loaded files; any error names the input it concerns.
fn load_inputs() -> Result<(PathBuf, StateParts), String> {
  let repo_root = match std::env::var_os(REPO_ROOT_ENV) {
    Some(value) if !value.is_empty() => PathBuf::from(value),
    _ => {
      return Err(format!("{REPO_ROOT_ENV} is not set; set it to the charlie-bot checkout the server serves"));
    }
  };
  if !repo_root.join("web").join("templates").is_dir() {
    return Err(format!(
      "{REPO_ROOT_ENV}={} holds no web/templates directory; set it to a charlie-bot checkout",
      repo_root.display()
    ));
  }
  let home = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let parts = StateParts { config: state::config_cache(&home), credentials: state::credentials_cache(&home) };
  let cfg = parts.config.get().map_err(|error| error.to_string())?;
  config::require_backends(&cfg).map_err(|error| error.to_string())?;
  VoiceSettings::from_config(&cfg)?;
  parts.credentials.get().map_err(|error| error.to_string())?;
  Ok((repo_root, parts))
}

/// uvicorn's `bind_socket`: an IPv6 socket when the host holds a `:`, IPv4 otherwise.
async fn bind(host: &str, port: u16) -> Result<TcpListener, String> {
  let address = if host.contains(':') {
    let ip: IpAddr = host.parse().map_err(|error| format!("server.host {host:?} is not an IPv6 address: {error}"))?;
    SocketAddr::new(ip, port)
  } else {
    (host, port)
      .to_socket_addrs()
      .map_err(|error| format!("resolving server.host {host:?} failed: {error}"))?
      .find(SocketAddr::is_ipv4)
      .ok_or_else(|| format!("server.host {host:?} has no IPv4 address"))?
  };
  TcpListener::bind(address)
    .await
    .map_err(|error| format!("error while attempting to bind on address {address}: {error}"))
}

/// Python's `round(seconds * 1000)` of a duration: milliseconds, half to even.
fn round_ms(duration: Duration) -> u64 {
  (duration.as_secs_f64() * 1000.0).round_ties_even() as u64
}

/// SIGTERM and SIGINT, installed before startup so a signal during startup stops the server once it is up.
struct StopSignal {
  terminate: tokio::signal::unix::Signal,
  interrupt: tokio::signal::unix::Signal,
}

impl StopSignal {
  fn install() -> std::io::Result<StopSignal> {
    Ok(StopSignal { terminate: signal(SignalKind::terminate())?, interrupt: signal(SignalKind::interrupt())? })
  }

  /// The first stop signal to arrive.
  async fn wait(&mut self) -> Signal {
    tokio::select! {
      _ = self.terminate.recv() => Signal::SIGTERM,
      _ = self.interrupt.recv() => Signal::SIGINT,
    }
  }
}
