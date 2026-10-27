//! Router, service, and request wiring contracts.

pub use crate::registry::{
  CheckFn, DiffRootFn, FileView, PathClaim, PathMatch, Phase, RouterBuilder, RouterRegistration, ScheduledHandlerFn,
  Service, ServiceContext, StartupCheckFn, WebSocketHandler, WebSocketMatch, WebSocketRequest,
};
