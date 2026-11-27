//! The paths of the host-auth routes, each spelled once. `api` mounts every route from its const, and `rows` derives
//! the standing page's URLs from the same consts, so a route rename has one spelling to change.

/// The route of a row's status JSON; `{id}` stands for the row's id.
pub const ROW_PATH: &str = "/api/host-auth/rows/{id}";
/// The route that checks one row now; `{id}` stands for the row's id.
pub const ROW_CHECK_PATH: &str = "/api/host-auth/rows/{id}/check";
/// The route that starts a login's browser flow; `{name}` stands for the login's config name.
pub const LOGIN_START_PATH: &str = "/api/host-auth/logins/{name}/start";
/// The route that submits a value to the login that waits for it; `{name}` stands for the login's config name.
pub const LOGIN_INPUT_PATH: &str = "/api/host-auth/logins/{name}/input";
