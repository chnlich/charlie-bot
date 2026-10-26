//! Wiring hooks (`src/runtime/hooks/wiring.py` and `scheduled_handlers.py`): routers, services, file views,
//! startup checks, config checks, diff roots and scheduled handlers.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_core::config::CharlieBotConfig;
use futures_util::future::{BoxFuture, Shared};

use crate::context::FeatureContext;
use crate::registry::{Registry, RegistryError};
use crate::route_claims::RouteClaims;

/// Builds a package's routes with their full paths, with the feature context applied as the router's state. The
/// server merges the routers in registration order.
pub type RouterBuilder = fn(&Arc<FeatureContext>) -> Router;

/// Whether a request path falls under one of a router's routes, whatever the method. The trailing-slash redirect
/// asks it of every router, as it asks the server's own route families.
pub type PathMatch = fn(&str) -> bool;

/// The request data a registered WebSocket route receives after the gate authorizes its upgrade.
pub struct WebSocketRequest {
  /// The decoded request path.
  pub path: String,
  /// The decoded path segments captured by `*`, in pattern order.
  pub segments: Vec<String>,
  /// The raw query string, or an empty string when the request has no query.
  pub query: String,
  /// The upgrade that the route accepts to start its WebSocket connection.
  pub upgrade: WebSocketUpgrade,
}

/// One WebSocket handler. The gate supplies the same feature context used to build package routers.
pub type WebSocketHandler = fn(Arc<FeatureContext>, WebSocketRequest) -> BoxFuture<'static, Response>;

/// A matching WebSocket registration and the path segments captured by its `*` wildcards.
pub struct WebSocketMatch {
  /// The registered handler.
  pub handler: WebSocketHandler,
  /// The decoded segments captured by `*`, in pattern order.
  pub segments: Vec<String>,
}

/// Which request paths a router claims, whatever the method. The trailing-slash redirect asks every router.
pub enum PathClaim {
  /// The route patterns the router mounts.
  Patterns(RouteClaims),
  /// A predicate over the decoded path, for a router whose claims are not route patterns.
  Predicate(PathMatch),
}

/// One registered router.
pub struct RouterRegistration {
  pub package: &'static str,
  pub build: RouterBuilder,
  pub claim: PathClaim,
}

impl RouterRegistration {
  /// Whether the router claims a decoded request path.
  pub fn claims(&self, path: &str) -> bool {
    match &self.claim {
      PathClaim::Patterns(routes) => routes.claims(path),
      PathClaim::Predicate(matches) => matches(path),
    }
  }
}

/// When a service starts: `Early` right after the crash-recovery step starts, `Ready` after the trigger step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
  Early,
  Ready,
}

/// What the lifespan hands each service's `start`.
pub struct ServiceContext {
  pub features: Arc<FeatureContext>,
  /// Resolves when the crash-recovery step ends. A service that reads recovered state awaits it.
  pub recovery_done: Shared<BoxFuture<'static, ()>>,
}

/// A background service (`wiring.register_service`). A service stops in reverse registration order, ready
/// services first. `stop` returns at once when `start` never ran.
pub trait Service: Send + Sync {
  fn name(&self) -> &'static str;
  fn phase(&self) -> Phase;
  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>>;
  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// A file view (`wiring.register_file_view`). The file server asks each view, in registration order, before it
/// serves a path; `path` is the absolute path without its leading "/". `Ok(Some)` answers the request, `Ok(None)`
/// passes it on, and `Err` is the error response (Python raises `HTTPException`).
pub trait FileView: Send + Sync {
  fn view<'a>(
    &'a self,
    features: &'a FeatureContext,
    request: &'a Parts,
    path: &'a str,
  ) -> BoxFuture<'a, Result<Option<Response>, Response>>;
}

/// A config check (`config_registry.register_config_check`) runs after every config load. It fails with its message.
pub type CheckFn = fn(&CharlieBotConfig) -> Result<(), String>;

/// A startup check (`wiring.register_startup_check`) runs once before the server serves. It reads the registry, so it
/// can resolve the usage sources and backend attributions the packages registered. It fails with its message.
pub type StartupCheckFn = fn(&Registry, &CharlieBotConfig) -> Result<(), String>;

/// A diff root (`wiring.register_diff_root`): the diff view also accepts repositories under the path it returns.
pub type DiffRootFn = fn(&CharlieBotConfig) -> PathBuf;

/// A scheduled handler (`scheduled_handlers.register_handler`): a cron task with `handler: <name>` awaits it, and
/// the `Ok` string is the fire's summary.
pub type ScheduledHandlerFn = fn(Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<String>>;

impl Registry {
  /// Register a router (`wiring.register_router`). Routers merge in registration order.
  pub fn router(&mut self, build: RouterBuilder, matches: PathMatch) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.routers.push(RouterRegistration { package, build, claim: PathClaim::Predicate(matches) });
    Ok(())
  }

  /// Register a router by the route patterns it mounts, in axum's syntax (`wiring.register_router`). Routers merge
  /// in registration order.
  pub fn router_patterns(&mut self, build: RouterBuilder, patterns: &[&str]) -> Result<(), RegistryError> {
    let package = self.owner()?;
    let claims = RouteClaims::new(patterns).map_err(|error| RegistryError { package, message: error.to_string() })?;
    self.routers.push(RouterRegistration { package, build, claim: PathClaim::Patterns(claims) });
    Ok(())
  }

  /// Register an authenticated WebSocket route. Each `*` matches one non-empty path segment.
  pub fn websocket(&mut self, pattern: &'static str, handler: WebSocketHandler) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.websocket_routes.insert(package, pattern, handler)
  }

  /// The first registered handler whose pattern matches `path`, with captured segments in pattern order.
  pub fn websocket_match(&self, path: &str) -> Option<WebSocketMatch> {
    let path_segments: Vec<&str> = path.split('/').collect();
    self.websocket_routes.pairs().find_map(|(pattern, handler)| {
      let pattern_segments: Vec<&str> = pattern.split('/').collect();
      if pattern_segments.len() != path_segments.len() {
        return None;
      }
      let mut segments = Vec::new();
      for (expected, actual) in pattern_segments.iter().zip(&path_segments) {
        if *expected == "*" {
          if actual.is_empty() {
            return None;
          }
          segments.push((*actual).to_string());
        } else if expected != actual {
          return None;
        }
      }
      Some(WebSocketMatch { handler: *handler, segments })
    })
  }

  /// Register a service under its own name (`wiring.register_service`).
  pub fn service(&mut self, service: Arc<dyn Service>) -> Result<(), RegistryError> {
    let package = self.owner()?;
    let name = service.name();
    self.services.insert(package, name, service)
  }

  /// Register a file view (`wiring.register_file_view`).
  pub fn file_view(&mut self, name: &'static str, view: Arc<dyn FileView>) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.file_views.insert(package, name, view)
  }

  /// Register a startup check (`wiring.register_startup_check`).
  pub fn startup_check(&mut self, name: &'static str, check: StartupCheckFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.startup_checks.insert(package, name, check)
  }

  /// Register a config check (`config_registry.register_config_check`).
  pub fn config_check(&mut self, name: &'static str, check: CheckFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.config_checks.insert(package, name, check)
  }

  /// Register a diff root (`wiring.register_diff_root`).
  pub fn diff_root(&mut self, name: &'static str, root: DiffRootFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.diff_roots.insert(package, name, root)
  }

  /// Register a scheduled handler under its task name (`scheduled_handlers.register_handler`).
  pub fn scheduled_handler(&mut self, name: &'static str, handler: ScheduledHandlerFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.scheduled_handlers.insert(package, name, handler)
  }

  /// The routers in registration order.
  pub fn routers(&self) -> &[RouterRegistration] {
    &self.routers
  }

  /// The services that start in `phase`, in registration order.
  pub fn services_in(&self, phase: Phase) -> Vec<Arc<dyn Service>> {
    self.services.values().filter(|service| service.phase() == phase).cloned().collect()
  }

  /// The file views in registration order.
  pub fn file_views(&self) -> impl Iterator<Item = (&'static str, &Arc<dyn FileView>)> {
    self.file_views.pairs()
  }

  /// The startup checks in registration order.
  pub fn startup_checks(&self) -> impl Iterator<Item = (&'static str, StartupCheckFn)> + '_ {
    self.startup_checks.pairs().map(|(name, check)| (name, *check))
  }

  /// The diff roots in registration order.
  pub fn diff_roots(&self) -> impl Iterator<Item = (&'static str, DiffRootFn)> + '_ {
    self.diff_roots.pairs().map(|(name, root)| (name, *root))
  }

  /// The scheduled handler registered under `name`, or `None` when no package registered it.
  pub fn scheduled_handler_named(&self, name: &str) -> Option<ScheduledHandlerFn> {
    self.scheduled_handlers.get(name).copied()
  }
}
