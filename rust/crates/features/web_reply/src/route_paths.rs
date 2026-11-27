//! The paths of the seven web reply routes, each spelled once. `entry` mounts the routes from these consts and claims
//! their paths for the server's trailing-slash redirect, and `cli` sends each verb's request to its const.

/// The route that records the body's text as the session's web reply.
pub const REPLY_PATH: &str = "/api/internal/web/reply";
/// The route that answers whether the reply text has been cold-read.
pub const REPLY_UNREAD_PATH: &str = "/api/internal/web/reply/unread";
/// The route that records one cold read of a reply, a Waiting on you item or the kept lists.
pub const COLD_READ_CHECK_PATH: &str = "/api/internal/web/cold-read-check";
/// The route that returns the session's For-you layer, in log order.
pub const DIALOG_PATH: &str = "/api/internal/web/dialog";
/// The route that stores the body's status and answers the stored status; it answers POST and GET.
pub const STATUS_PATH: &str = "/api/internal/web/status";
/// The route that confirms the calling session's stored status needs no change.
pub const STATUS_CONFIRM_PATH: &str = "/api/internal/web/status/confirm";
/// The route that returns the waiting items that no cold-read event covers.
pub const STATUS_UNREAD_PATH: &str = "/api/internal/web/status/unread";

/// Every web reply route path, which the server's trailing-slash redirect claims.
pub const PATHS: [&str; 7] = [
  REPLY_PATH,
  REPLY_UNREAD_PATH,
  COLD_READ_CHECK_PATH,
  DIALOG_PATH,
  STATUS_PATH,
  STATUS_CONFIRM_PATH,
  STATUS_UNREAD_PATH,
];
