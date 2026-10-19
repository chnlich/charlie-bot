//! `charliebot-server`: the CharlieBot HTTP server (`server.py`).
//!
//! Startup follows `server.main` then the lifespan: replace the process environment with the agent environment before
//! any thread starts, build the registry from the composition root, load and check the config, take the home writer
//! fence (another holder exits 3 after logging `server_startup_refused_home_writer_fence`), capture the build identity,
//! initialize the home, log `charliebot_home_ready`, run the lifespan's startup steps, log `server_ready`, then bind the
//! configured host and port. SIGTERM or SIGINT stops accepting, lets in-flight requests finish for up to five seconds,
//! stops the lifespan's services, runs its let-go steps, closes the shared HTTP client and the open WebSockets,
//! releases the fence, logs `charliebot_shutdown`, then raises the signal again under its default disposition, so the
//! process ends killed by that signal as uvicorn's does.
//!
//! The repo root variable (`charliebot_core::repo_root::REPO_ROOT_ENV`) names the charlie-bot checkout whose `web/`,
//! page packages and `configs/` the server serves; the Python server derives the same root from its own file location.

mod app;
mod auth;
mod buildinfo;
mod cookies;
mod gzip;
mod home_init;
mod packages;
mod request_log;
mod request_path;
mod routes;
mod state;
mod store_setup;

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use charliebot_agents::agent_environment::{self, AgentBaseEnvironment};
use charliebot_compat::mimetypes::MimeTypes;
use charliebot_core::fence::{self, FenceError};
use charliebot_core::repo_root::{REPO_ROOT_ENV, RepoRoot};
use charliebot_core::venv::VenvBin;
use charliebot_core::{config, home, logging, paths, v1_sessions};
use charliebot_registry::lifespan::{CloseTimings, Lifespan};
use charliebot_registry::templating::Templating;
use charliebot_registry::{FeatureContext, Registry, StartupState};
use nix::sys::signal::{SigHandler, Signal};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::app::ConnectionInfo;
use crate::buildinfo::BuildInfo;
use crate::routes::static_mounts::StaticMount;
use crate::state::AppState;

/// `timeouts.SERVER_GRACEFUL_SHUTDOWN_TIMEOUT`: how long in-flight requests get after a stop signal.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// A config or environment error (Python's `main` raising).
const EXIT_CONFIG: u8 = 1;
/// A lifespan startup failure (uvicorn's "Application startup failed").
const EXIT_STARTUP: u8 = 3;

fn main() -> ExitCode {
  // Before the logger and the runtime start, so the environment changes while this is the only thread.
  let (venv_bin, repo_root, agent_base) = match replace_environment() {
    Ok(inputs) => inputs,
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  logging::init();
  let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("the tokio runtime builds");
  runtime.block_on(run(venv_bin, repo_root, agent_base))
}

async fn run(venv_bin: VenvBin, repo_root: RepoRoot, agent_base: AgentBaseEnvironment) -> ExitCode {
  let mut stop = match StopSignal::install() {
    Ok(stop) => stop,
    Err(error) => {
      eprintln!("charliebot-server: installing the signal handlers failed: {error}");
      return ExitCode::from(EXIT_STARTUP);
    }
  };
  let boot = Instant::now();
  let registry = match packages::registry() {
    Ok(registry) => Arc::new(registry),
    Err(error) => {
      eprintln!("charliebot-server: {error}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  let state_parts = match load_inputs(&registry, &repo_root) {
    Ok(parts) => parts,
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

  let build_info = BuildInfo::capture(repo_root.path());
  if let Err(error) = home_init::init_charliebot_home(&cfg, repo_root.path()) {
    writer_fence.release();
    eprintln!("charliebot-server: initializing {} failed: {error}", paths::display(&cfg.charliebot_home));
    return ExitCode::from(EXIT_STARTUP);
  }
  let store = match store_setup::create((*cfg).clone(), Arc::clone(&registry)) {
    Ok(store) => store,
    Err(error) => {
      writer_fence.release();
      eprintln!("charliebot-server: creating session store failed: {error}");
      return ExitCode::from(EXIT_STARTUP);
    }
  };
  tracing::info!(path = %paths::display(&cfg.charliebot_home), "charliebot_home_ready");
  let startup = StartupState::default();
  startup.insert(venv_bin).expect("the startup state is empty before the lifespan begins");
  startup.insert(repo_root.clone()).expect("the startup state is empty before the lifespan begins");
  startup.insert(agent_base).expect("the startup state is empty before the lifespan begins");
  startup
    .insert(Templating::new(repo_root.path(), registry.page_packages()))
    .expect("the startup state is empty before the lifespan begins");
  let features = Arc::new(FeatureContext {
    config: state_parts.config,
    credentials: state_parts.credentials,
    store: Arc::new(store),
    registry: Arc::clone(&registry),
    startup: Arc::new(startup),
  });
  // The lifespan's recovery can re-attach a round whose master_done starts the after-turn works, so the context is
  // bound before the lifespan starts.
  registry.bind_feature_context(&features).expect("the registry binds its feature context once, at startup");
  let lifespan = match Lifespan::start(Arc::clone(&features)).await {
    Ok(lifespan) => lifespan,
    Err(error) => {
      writer_fence.release();
      eprintln!("charliebot-server: lifespan startup failed: {error:#}");
      return ExitCode::from(EXIT_STARTUP);
    }
  };
  let state = Arc::new(AppState {
    features,
    build_info,
    static_mounts: StaticMount::python_app_mounts(repo_root.path(), registry.page_packages()),
    mime_types: MimeTypes::load(),
  });
  tracing::info!(ready_in_ms = round_ms(boot.elapsed()), "server_ready");

  let listener = match bind(&cfg.server.host, cfg.server.port).await {
    Ok(listener) => listener,
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      let shutdown_started = Instant::now();
      let closes = stop_lifespan(lifespan).await;
      writer_fence.release();
      log_shutdown(shutdown_started, closes);
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
  let closes = stop_lifespan(lifespan).await;
  writer_fence.release();
  log_shutdown(shutdown_started, closes);
  exit_by_signal(received)
}

/// The lifespan's shutdown. A failed stop is logged, and the caller still releases the home fence.
async fn stop_lifespan(lifespan: Lifespan) -> CloseTimings {
  let shutdown = lifespan.shutdown().await;
  if let Some(error) = shutdown.error {
    tracing::error!(error = format!("{error:#}"), "lifespan_shutdown_failed");
  }
  shutdown.closes
}

/// The `charliebot_shutdown` line: the shutdown's total time and the time of each close, as Python's lifespan logs them.
fn log_shutdown(shutdown_started: Instant, closes: CloseTimings) {
  tracing::info!(
    shutdown_ms = round_ms(shutdown_started.elapsed()),
    http_client_ms = round_ms(closes.http_client),
    ws_close_ms = round_ms(closes.ws_close),
    "charliebot_shutdown"
  );
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
  config: Arc<charliebot_core::hot_reload::HotReload<config::CharlieBotConfig>>,
  credentials: Arc<charliebot_core::hot_reload::HotReload<charliebot_core::credentials::Credentials>>,
}

/// The venv bin and the checkout, read once, and the process environment rewritten from them. Any error names the input
/// it concerns.
fn replace_environment() -> Result<(VenvBin, RepoRoot, AgentBaseEnvironment), String> {
  let venv_bin = VenvBin::new(std::env::var_os("CHARLIEBOT_VENV_BIN")).map_err(|error| error.to_string())?;
  let repo_root = RepoRoot::new(std::env::var_os(REPO_ROOT_ENV)).map_err(|error| error.to_string())?;
  let agent_base =
    agent_environment::replace_process_environment(&venv_bin, &repo_root).map_err(|error| format!("{error:#}"))?;
  Ok((venv_bin, repo_root, agent_base))
}

/// The home's loaded files, checked against the checkout; any error names the input it concerns.
fn load_inputs(registry: &Arc<Registry>, repo_root: &RepoRoot) -> Result<StateParts, String> {
  if !repo_root.path().join("web").join("templates").is_dir() {
    return Err(format!(
      "{REPO_ROOT_ENV}={} holds no web/templates directory; set it to a charlie-bot checkout",
      repo_root.path().display()
    ));
  }
  let home = home::charliebot_home_dir().map_err(|error| error.to_string())?;
  let parts = StateParts {
    config: state::config_cache(&home, Arc::clone(registry)),
    credentials: state::credentials_cache(&home),
  };
  let cfg = parts.config.get().map_err(|error| error.to_string())?;
  config::require_backends(&cfg).map_err(|error| error.to_string())?;
  v1_sessions::require_no_v1_sessions(&cfg)?;
  for (_, check) in registry.startup_checks() {
    check(registry, &cfg)?;
  }
  parts.credentials.get().map_err(|error| error.to_string())?;
  Ok(parts)
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

#[cfg(test)]
mod tests {
  use std::fmt;
  use std::sync::{Arc, Mutex};

  use tracing::field::{Field, Visit};
  use tracing::{Event, Subscriber};
  use tracing_subscriber::layer::{Context, SubscriberExt};
  use tracing_subscriber::{Layer, Registry};

  use super::*;

  /// The `(name, value)` pairs of one event, in the order the event records them.
  type Recorded = Vec<(String, String)>;

  #[derive(Default)]
  struct Visitor(Recorded);

  impl Visit for Visitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
      self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
      self.0.push((field.name().to_string(), format!("{value:?}")));
    }
  }

  struct Capture(Arc<Mutex<Vec<Recorded>>>);

  impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
      let mut visitor = Visitor::default();
      event.record(&mut visitor);
      self.0.lock().unwrap().push(visitor.0);
    }
  }

  #[test]
  fn shutdown_line_reports_the_shutdown_and_each_close() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = Registry::default().with(Capture(Arc::clone(&events)));
    tracing::subscriber::with_default(subscriber, || {
      let closes = CloseTimings { http_client: Duration::from_millis(3), ws_close: Duration::from_millis(5) };
      log_shutdown(Instant::now(), closes);
    });

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let names: Vec<&str> = events[0].iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["message", "shutdown_ms", "http_client_ms", "ws_close_ms"]);
    assert_eq!(events[0][0].1, "charliebot_shutdown");
    assert_eq!(events[0][2].1, "3");
    assert_eq!(events[0][3].1, "5");
  }
}
