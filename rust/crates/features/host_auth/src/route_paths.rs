//! The paths of the four host-auth row and login routes, each spelled once. `api` mounts these routes from the consts,
//! and `rows` derives the standing page's URLs from the same consts.
//!
//! The module holds no other host-auth path. `api` still mounts the page, status and probe routes from inline literals,
//! and its trailing-slash redirect spells the row and login prefixes and action names itself. The page's URL builders
//! also substitute the `{id}` and `{name}` placeholders, so a route rename changes those sites as well.

/// The route of a row's status JSON; `{id}` stands for the row's id.
pub const ROW_PATH: &str = "/api/host-auth/rows/{id}";
/// The route that checks one row now; `{id}` stands for the row's id.
pub const ROW_CHECK_PATH: &str = "/api/host-auth/rows/{id}/check";
/// The route that starts a login's browser flow; `{name}` stands for the login's config name.
pub const LOGIN_START_PATH: &str = "/api/host-auth/logins/{name}/start";
/// The route that submits a value to the login that waits for it; `{name}` stands for the login's config name.
pub const LOGIN_INPUT_PATH: &str = "/api/host-auth/logins/{name}/input";
