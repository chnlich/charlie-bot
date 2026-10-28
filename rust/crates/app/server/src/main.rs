//! `charliebot-server`: the CharlieBot HTTP server (`server.py`).
//!
//! Startup follows `server.main` then the lifespan: replace the process environment with the agent environment before
//! any thread starts, build the registry from the composition root, load and check the config, take the home writer
//! fence (another holder exits 3 after logging `server_startup_refused_home_writer_fence`), capture the build identity,
//! initialize the home, log `charliebot_home_ready`, run the lifespan's startup steps, log `server_ready`, then bind the
//! configured host and port. SIGINT and SIGTERM are installed before startup. A boot stop waits for the current safe
//! point, stops started services, closes shared clients, releases the fence and logs `charliebot_shutdown` with
//! `boot=true`. After `server_ready`, a stop signal drains requests for up to five seconds, stops services, closes
//! shared clients, releases the fence and logs `charliebot_shutdown` (`server.py:293-310`). SIGTERM then restores its
//! default disposition and raises SIGTERM without let-go steps. SIGINT runs let-go steps after the log and returns 0.
//! A failed bind runs let-go steps after the log and returns 1 (`uvicorn/server.py:179-182`). Uvicorn re-raises SIGTERM;
//! normal Python exits cancel tasks and Uvicorn catches KeyboardInterrupt (`uvicorn/server.py:322-339`,
//! `uvicorn/main.py:607-608`, `asyncio/runners.py:66-75`).
//!
//! The repo root variable (`charliebot_core::repo_root::REPO_ROOT_ENV`) names the charlie-bot checkout whose `web/`,
//! page packages and `configs/` the server serves; the Python server derives the same root from its own file location.

mod app;
mod auth;
mod buildinfo;
mod connection;
mod cookies;
mod gzip;
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
use charliebot_core::{config, home, logging, paths};
use charliebot_registry::lifespan::{CloseTimings, Lifespan, StartupOutcome, StepTiming};
use charliebot_registry::templating::Templating;
use charliebot_registry::{FeatureContext, Registry, StartupState, StartupStop};
use charliebot_store::v1_sessions;
use nix::sys::signal::{SigHandler, Signal};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::buildinfo::BuildInfo;
use crate::connection::ConnectionInfo;
use crate::state::AppState;
use crate::state::StaticMount;

/// `timeouts.SERVER_GRACEFUL_SHUTDOWN_TIMEOUT`: how long in-flight requests get after a stop signal.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// A config or environment error (Python's `main` raising).
const EXIT_CONFIG: u8 = 1;
/// An unsupported command-line argument.
const EXIT_USAGE: u8 = 2;
/// A lifespan startup failure (uvicorn's "Application startup failed").
const EXIT_STARTUP: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
  Signal(Signal),
  BindFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopAction {
  /// Stops services and closes the shared clients.
  Shutdown,
  ReleaseWriterFence,
  LogShutdown,
  RunLetGo,
  RaiseSignal(Signal),
  ReturnStatus(u8),
}

fn stop_sequence(reason: StopReason) -> Vec<StopAction> {
  match reason {
    StopReason::Signal(Signal::SIGTERM) => vec![
      StopAction::Shutdown,
      StopAction::ReleaseWriterFence,
      StopAction::LogShutdown,
      StopAction::RaiseSignal(Signal::SIGTERM),
    ],
    StopReason::Signal(Signal::SIGINT) => vec![
      StopAction::Shutdown,
      StopAction::ReleaseWriterFence,
      StopAction::LogShutdown,
      StopAction::RunLetGo,
      StopAction::ReturnStatus(0),
    ],
    StopReason::Signal(signal) => unreachable!("the stop handler only receives SIGTERM or SIGINT, got {signal}"),
    StopReason::BindFailure => vec![
      StopAction::Shutdown,
      StopAction::ReleaseWriterFence,
      StopAction::LogShutdown,
      StopAction::RunLetGo,
      StopAction::ReturnStatus(EXIT_CONFIG),
    ],
  }
}

fn main() -> ExitCode {
  let args: Vec<_> = std::env::args_os().skip(1).collect();
  if args.first().is_some_and(|arg| arg == charliebot_opencode::relay::RELAY_ARG) {
    return match charliebot_opencode::relay::run_args(&args[1..]) {
      Ok(()) => ExitCode::SUCCESS,
      Err(error) => {
        eprintln!("OpenCode relay: {error:#}");
        ExitCode::FAILURE
      }
    };
  }
  if args.first().is_some_and(|arg| arg == charliebot_agents::transport::final_stdout::SUPERVISOR_ARG) {
    return match charliebot_agents::transport::final_stdout::supervise(&args[1..]) {
      Ok(()) => ExitCode::SUCCESS,
      Err(error) => {
        eprintln!("final-stdout supervisor: {error:#}");
        ExitCode::FAILURE
      }
    };
  }
  let mut arguments = std::env::args_os().skip(1);
  match (arguments.next(), arguments.next()) {
    (None, None) => run_server(),
    (Some(argument), None) if argument == std::ffi::OsStr::new("--check-config") => check_config(),
    _ => {
      eprintln!("Usage: charliebot-server [--check-config]");
      ExitCode::from(EXIT_USAGE)
    }
  }
}

fn check_config() -> ExitCode {
  let (repo_root, _) = match replace_environment() {
    Ok(inputs) => inputs,
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  let registry = match packages::registry() {
    Ok(registry) => Arc::new(registry),
    Err(error) => {
      eprintln!("charliebot-server: {error}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  match load_inputs(&registry, &repo_root) {
    Ok(_) => {
      println!("config ok");
      ExitCode::SUCCESS
    }
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      ExitCode::from(EXIT_CONFIG)
    }
  }
}

fn run_server() -> ExitCode {
  // Before the logger and the runtime start, so the environment changes while this is the only thread.
  let (repo_root, agent_base) = match replace_environment() {
    Ok(inputs) => inputs,
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  logging::init();
  let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("the tokio runtime builds");
  runtime.block_on(run(repo_root, agent_base))
}

async fn run(repo_root: RepoRoot, agent_base: AgentBaseEnvironment) -> ExitCode {
  let mut stop = match StopSignal::install() {
    Ok(stop) => stop,
    Err(error) => {
      eprintln!("charliebot-server: installing the signal handlers failed: {error}");
      return ExitCode::from(EXIT_STARTUP);
    }
  };
  let startup_stop = StartupStop::default();
  let monitor_stop = startup_stop.clone();
  let (signal_sender, signal_receiver) = tokio::sync::oneshot::channel();
  tokio::spawn(async move {
    let received = stop.wait().await;
    monitor_stop.cancel();
    let _ = signal_sender.send(received);
  });
  let boot = Instant::now();
  let registry = match packages::registry() {
    Ok(registry) => Arc::new(registry),
    Err(error) => {
      eprintln!("charliebot-server: {error}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  if startup_stop.is_cancelled() {
    return finish_boot_stop(None, None, signal_receiver).await;
  }
  let state_parts = match load_inputs(&registry, &repo_root) {
    Ok(parts) => parts,
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      return ExitCode::from(EXIT_CONFIG);
    }
  };
  if startup_stop.is_cancelled() {
    return finish_boot_stop(None, None, signal_receiver).await;
  }
  let cfg = state_parts.config.get().expect("load_inputs loaded the config");

  if startup_stop.is_cancelled() {
    return finish_boot_stop(None, None, signal_receiver).await;
  }
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
  if startup_stop.is_cancelled() {
    return finish_boot_stop(None, Some(&mut writer_fence), signal_receiver).await;
  }

  let build_info = BuildInfo::capture(repo_root.path());
  if let Err(error) = charliebot_core::home_init::init_charliebot_home(&cfg, repo_root.path()) {
    writer_fence.release();
    eprintln!("charliebot-server: initializing {} failed: {error}", paths::display(&cfg.charliebot_home));
    return ExitCode::from(EXIT_STARTUP);
  }
  if startup_stop.is_cancelled() {
    return finish_boot_stop(None, Some(&mut writer_fence), signal_receiver).await;
  }
  let store = match store_setup::create((*cfg).clone(), Arc::clone(&registry)) {
    Ok(store) => store,
    Err(error) => {
      writer_fence.release();
      eprintln!("charliebot-server: creating session store failed: {error}");
      return ExitCode::from(EXIT_STARTUP);
    }
  };
  if startup_stop.is_cancelled() {
    return finish_boot_stop(None, Some(&mut writer_fence), signal_receiver).await;
  }
  tracing::info!(path = %paths::display(&cfg.charliebot_home), "charliebot_home_ready");
  let startup = StartupState::with_stop(startup_stop.clone());
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
  let mut lifespan = match Lifespan::start(Arc::clone(&features)).await {
    Ok(StartupOutcome::Ready(lifespan)) => lifespan,
    Ok(StartupOutcome::Stopped(mut lifespan)) => {
      return finish_boot_stop(Some(&mut lifespan), Some(&mut writer_fence), signal_receiver).await;
    }
    Err(error) => {
      writer_fence.release();
      eprintln!("charliebot-server: lifespan startup failed: {error:#}");
      return ExitCode::from(EXIT_STARTUP);
    }
  };
  if startup_stop.is_cancelled() {
    return finish_boot_stop(Some(&mut lifespan), Some(&mut writer_fence), signal_receiver).await;
  }
  let state = Arc::new(AppState {
    features,
    build_info,
    static_mounts: StaticMount::python_app_mounts(repo_root.path(), registry.page_packages()),
    mime_types: MimeTypes::load(),
  });
  if !startup_stop.mark_ready() {
    return finish_boot_stop(Some(&mut lifespan), Some(&mut writer_fence), signal_receiver).await;
  }
  // Python logs `server_ready` after starting its ready-phase services and yields straight after (`server.py:285-291`).
  // The host_auth poller's 300 ms startup tail keeps its first state read after this line while preserving the stop
  // check: a boot stopped before readiness does not emit `server_ready`.
  tracing::info!(ready_in_ms = round_ms(boot.elapsed()), "server_ready");

  let listener = match bind(&cfg.server.host, cfg.server.port).await {
    Ok(listener) => listener,
    Err(message) => {
      eprintln!("charliebot-server: {message}");
      return finish_stop(&mut lifespan, &mut writer_fence, Instant::now(), StopReason::BindFailure).await;
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
  let received = signal_receiver.await.expect("the signal monitor sends the first stop signal");
  // The receiver lives until the server task ends; a send to an ended server has nothing to stop.
  let _ = stop_sender.send(());
  match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, server).await {
    Ok(Ok(Ok(()))) => {}
    Ok(Ok(Err(error))) => tracing::error!(error = %error, "server_failed"),
    Ok(Err(join_error)) => tracing::error!(error = %join_error, "server_failed"),
    Err(_) => tracing::warn!(timeout_s = GRACEFUL_SHUTDOWN_TIMEOUT.as_secs(), "graceful_shutdown_timeout_exceeded"),
  }
  finish_stop(&mut lifespan, &mut writer_fence, Instant::now(), StopReason::Signal(received)).await
}

async fn finish_stop(
  lifespan: &mut Lifespan,
  writer_fence: &mut fence::HomeWriterFence,
  shutdown_started: Instant,
  reason: StopReason,
) -> ExitCode {
  let mut closes = None;
  for action in stop_sequence(reason) {
    match action {
      StopAction::Shutdown => closes = Some(stop_lifespan(lifespan).await),
      StopAction::ReleaseWriterFence => writer_fence.release(),
      StopAction::LogShutdown => {
        log_shutdown(shutdown_started, closes.take().expect("shutdown runs before its log line"), false);
      }
      StopAction::RunLetGo => {
        if let Err(error) = lifespan.run_let_go_steps().await {
          tracing::error!(error = format!("{error:#}"), "lifespan_shutdown_failed");
        }
      }
      StopAction::RaiseSignal(signal) => return exit_by_signal(signal),
      StopAction::ReturnStatus(status) => return ExitCode::from(status),
    }
  }
  unreachable!("every stop sequence ends by raising a signal or returning an exit status")
}

async fn finish_boot_stop(
  lifespan: Option<&mut Lifespan>,
  writer_fence: Option<&mut fence::HomeWriterFence>,
  signal_receiver: tokio::sync::oneshot::Receiver<Signal>,
) -> ExitCode {
  let received = signal_receiver.await.expect("the signal monitor sends the first stop signal");
  let shutdown_started = Instant::now();
  let mut lifespan = lifespan;
  let mut writer_fence = writer_fence;
  let mut closes = None;
  for action in stop_sequence(StopReason::Signal(received)) {
    match action {
      StopAction::Shutdown => {
        closes = Some(match lifespan.as_deref_mut() {
          Some(lifespan) => stop_lifespan(lifespan).await,
          None => CloseTimings::default(),
        });
      }
      StopAction::ReleaseWriterFence => {
        if let Some(writer_fence) = writer_fence.as_deref_mut() {
          writer_fence.release();
        }
      }
      StopAction::LogShutdown => {
        log_shutdown(shutdown_started, closes.take().expect("shutdown runs before its log line"), true);
      }
      StopAction::RunLetGo => {
        if let Some(lifespan) = lifespan.as_deref_mut()
          && let Err(error) = lifespan.run_let_go_steps().await
        {
          tracing::error!(error = format!("{error:#}"), "lifespan_shutdown_failed");
        }
      }
      StopAction::RaiseSignal(signal) => return exit_by_signal(signal),
      StopAction::ReturnStatus(status) => return ExitCode::from(status),
    }
  }
  unreachable!("every boot stop sequence ends by raising a signal or returning an exit status")
}

/// The lifespan's shutdown. A failed stop is logged, and the caller still releases the home fence.
async fn stop_lifespan(lifespan: &mut Lifespan) -> CloseTimings {
  let shutdown = lifespan.shutdown().await;
  if let Some(error) = shutdown.error {
    tracing::error!(error = format!("{error:#}"), "lifespan_shutdown_failed");
  }
  shutdown.closes
}

/// The `charliebot_shutdown` line: the total shutdown time and Python's service and shared-client timings.
fn log_shutdown(shutdown_started: Instant, closes: CloseTimings, during_boot: bool) {
  if during_boot {
    tracing::info!(
      boot = true,
      shutdown_ms = round_ms(shutdown_started.elapsed()),
      discord_ms = round_ms(step_elapsed(&closes.step_closes, "discord")),
      ext_usage_ms = round_ms(step_elapsed(&closes.step_closes, "ext_usage")),
      host_auth_ms = round_ms(step_elapsed(&closes.step_closes, "host_auth")),
      merge_pool_ms = round_ms(step_elapsed(&closes.step_closes, "merge_pool")),
      scheduler_ms = round_ms(step_elapsed(&closes.step_closes, "scheduler")),
      slack_ms = round_ms(step_elapsed(&closes.step_closes, "slack")),
      // Python stops `usage_tally`; Rust has no `usage_tally` service.
      usage_tally_ms = 0,
      http_client_ms = round_ms(closes.http_client),
      ws_close_ms = round_ms(closes.ws_close),
      "charliebot_shutdown"
    );
  } else {
    tracing::info!(
      shutdown_ms = round_ms(shutdown_started.elapsed()),
      discord_ms = round_ms(step_elapsed(&closes.step_closes, "discord")),
      ext_usage_ms = round_ms(step_elapsed(&closes.step_closes, "ext_usage")),
      host_auth_ms = round_ms(step_elapsed(&closes.step_closes, "host_auth")),
      merge_pool_ms = round_ms(step_elapsed(&closes.step_closes, "merge_pool")),
      scheduler_ms = round_ms(step_elapsed(&closes.step_closes, "scheduler")),
      slack_ms = round_ms(step_elapsed(&closes.step_closes, "slack")),
      // Python stops `usage_tally`; Rust has no `usage_tally` service.
      usage_tally_ms = 0,
      http_client_ms = round_ms(closes.http_client),
      ws_close_ms = round_ms(closes.ws_close),
      "charliebot_shutdown"
    );
  }
}

fn step_elapsed(step_closes: &[StepTiming], name: &'static str) -> Duration {
  step_closes.iter().find(|step| step.name == name).map_or(Duration::ZERO, |step| step.elapsed)
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

/// The checkout, read once, and the process environment rewritten from it. Any error names the input it concerns.
fn replace_environment() -> Result<(RepoRoot, AgentBaseEnvironment), String> {
  let repo_root = RepoRoot::new(std::env::var_os(REPO_ROOT_ENV)).map_err(|error| error.to_string())?;
  let server_executable =
    std::env::current_exe().map_err(|error| format!("cannot resolve charliebot-server path: {error}"))?;
  let server_bin_dir = server_executable
    .parent()
    .ok_or_else(|| format!("charliebot-server path has no parent: {}", server_executable.display()))?;
  let agent_base =
    agent_environment::replace_process_environment(server_bin_dir).map_err(|error| format!("{error:#}"))?;
  Ok((repo_root, agent_base))
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

/// SIGTERM and SIGINT, installed before startup. A signal during startup stops startup at the next safe point, before
/// `server_ready` is logged.
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
  use std::path::Path;
  use std::sync::{Arc, Mutex};

  use charliebot_core::test_support::EventSink;
  use charliebot_registry::wiring::{Phase, Service, ServiceContext};
  use futures_util::future::BoxFuture;
  use tracing::Event;
  use tracing::field::{Field, Visit};

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

  impl EventSink for Capture {
    fn event(&self, event: &Event<'_>) {
      let mut visitor = Visitor::default();
      event.record(&mut visitor);
      self.0.lock().unwrap().push(visitor.0);
    }
  }

  #[test]
  // Python: server.py:293-310.
  fn shutdown_line_has_python_fields_and_each_close_is_numeric() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let _capture = charliebot_core::test_support::install_events(Arc::new(Capture(Arc::clone(&events))));
    let step_closes = ["discord", "ext_usage", "host_auth", "merge_pool", "scheduler", "slack"]
      .map(|name| StepTiming { name, elapsed: Duration::ZERO })
      .into();
    let closes =
      CloseTimings { step_closes, http_client: Duration::from_millis(3), ws_close: Duration::from_millis(5) };
    log_shutdown(Instant::now(), closes, false);

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let mut names: Vec<&str> = events[0].iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(events[0][0].1, "charliebot_shutdown");
    assert_eq!(
      names,
      [
        "discord_ms",
        "ext_usage_ms",
        "host_auth_ms",
        "http_client_ms",
        "merge_pool_ms",
        "message",
        "scheduler_ms",
        "shutdown_ms",
        "slack_ms",
        "usage_tally_ms",
        "ws_close_ms",
      ]
    );
    for name in [
      "shutdown_ms",
      "discord_ms",
      "ext_usage_ms",
      "host_auth_ms",
      "merge_pool_ms",
      "scheduler_ms",
      "slack_ms",
      "usage_tally_ms",
      "http_client_ms",
      "ws_close_ms",
    ] {
      let value = events[0]
        .iter()
        .find_map(|(field, value)| (field == name).then_some(value))
        .unwrap_or_else(|| panic!("the shutdown line has no {name} field; fields: {names:?}"));
      assert!(value.parse::<u64>().is_ok(), "{name}={value} is not a non-negative integer");
    }
    assert_eq!(events[0].iter().find(|(name, _)| name == "http_client_ms").unwrap().1, "3");
    assert_eq!(events[0].iter().find(|(name, _)| name == "ws_close_ms").unwrap().1, "5");
  }

  #[test]
  fn sigterm_stop_skips_let_go_and_raises_sigterm_after_shutdown_logging() {
    assert_eq!(
      stop_sequence(StopReason::Signal(Signal::SIGTERM)),
      vec![
        StopAction::Shutdown,
        StopAction::ReleaseWriterFence,
        StopAction::LogShutdown,
        StopAction::RaiseSignal(Signal::SIGTERM),
      ]
    );
  }

  #[test]
  fn sigint_stop_runs_let_go_after_shutdown_logging_and_returns_zero() {
    assert_eq!(
      stop_sequence(StopReason::Signal(Signal::SIGINT)),
      vec![
        StopAction::Shutdown,
        StopAction::ReleaseWriterFence,
        StopAction::LogShutdown,
        StopAction::RunLetGo,
        StopAction::ReturnStatus(0),
      ]
    );
  }

  #[test]
  fn failed_bind_runs_let_go_after_shutdown_logging_and_returns_one() {
    assert_eq!(
      stop_sequence(StopReason::BindFailure),
      vec![
        StopAction::Shutdown,
        StopAction::ReleaseWriterFence,
        StopAction::LogShutdown,
        StopAction::RunLetGo,
        StopAction::ReturnStatus(1),
      ]
    );
  }

  /// How long the test service's stop takes, in milliseconds.
  const SLOW_STOP_MS: u64 = 200;

  /// A service whose stop takes `SLOW_STOP_MS`, so the shutdown line has a known delay to cover.
  struct SlowStop;

  impl Service for SlowStop {
    fn name(&self) -> &'static str {
      "slow_stop"
    }

    fn phase(&self) -> Phase {
      Phase::Early
    }

    fn start<'a>(&'a self, _ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async { Ok(()) })
    }

    fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>> {
      Box::pin(async {
        tokio::time::sleep(Duration::from_millis(SLOW_STOP_MS)).await;
        Ok(())
      })
    }
  }

  fn slow_stop_package(registry: &mut charliebot_registry::Registry) -> Result<(), charliebot_registry::RegistryError> {
    registry.service(Arc::new(SlowStop))
  }

  /// The feature context over the synthetic home `home`, assembled as `run` assembles it.
  fn features_over(home: &Path, registry: Arc<charliebot_registry::Registry>) -> Arc<FeatureContext> {
    let cfg = registry.load_config(home).expect("the synthetic config loads");
    Arc::new(FeatureContext {
      config: state::config_cache(home, Arc::clone(&registry)),
      credentials: state::credentials_cache(home),
      store: Arc::new(store_setup::create(cfg, Arc::clone(&registry)).expect("the store opens")),
      registry,
      startup: Arc::new(StartupState::with_stop(StartupStop::default())),
    })
  }

  #[tokio::test]
  async fn boot_stop_shutdown_line_covers_a_slow_service_stop() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let _capture = charliebot_core::test_support::install_events(Arc::new(Capture(Arc::clone(&events))));
    let home = tempfile::tempdir().expect("the synthetic home is created");
    std::fs::write(home.path().join("config.yaml"), "").expect("the synthetic config is written");
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.features.slow_stop", slow_stop_package).expect("the test service registers");
    let features = features_over(home.path(), Arc::new(registry));
    let mut lifespan = match Lifespan::start(features).await.expect("the lifespan starts") {
      StartupOutcome::Ready(lifespan) => lifespan,
      StartupOutcome::Stopped(_) => panic!("startup stopped without a stop signal"),
    };

    let (sender, receiver) = tokio::sync::oneshot::channel();
    sender.send(Signal::SIGINT).expect("the boot stop receives the signal");
    finish_boot_stop(Some(&mut lifespan), None, receiver).await;

    let events = events.lock().unwrap();
    let shutdown = events
      .iter()
      .find(|event| event.iter().any(|(name, value)| name == "message" && value == "charliebot_shutdown"))
      .expect("the boot stop logs its shutdown line");
    let shutdown_ms = shutdown
      .iter()
      .find_map(|(name, value)| (name == "shutdown_ms").then_some(value))
      .expect("the shutdown line has a shutdown_ms field");
    let shutdown_ms: u64 = shutdown_ms.parse().expect("shutdown_ms is a whole number of milliseconds");
    assert!(shutdown_ms >= SLOW_STOP_MS, "shutdown_ms {shutdown_ms} does not cover the {SLOW_STOP_MS} ms stop");
  }
}
