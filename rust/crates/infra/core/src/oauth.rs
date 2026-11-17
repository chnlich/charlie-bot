//! The OAuth 2.0 token vocabulary: the fields of a token request and of the token response a client reads back.
//!
//! Three flows speak it: the Google account CLI's authorization-code exchange and refresh, Claude Code's
//! usage-quota refresh, and Feishu's user-token exchange. The names are wire fields of the providers' endpoints.
//! Two of them also key the credentials file: `client_id` and `refresh_token`.

/// The request field that names the grant a client asks for.
pub const GRANT_TYPE_FIELD: &str = "grant_type";
/// The `grant_type` value of a refresh-token request.
pub const REFRESH_TOKEN_GRANT: &str = "refresh_token";
/// The `grant_type` value of an authorization-code exchange.
pub const AUTHORIZATION_CODE_GRANT: &str = "authorization_code";
/// The refresh token: the request field that carries one, the stored credential that keeps one, and the response field
/// that returns a rotated one.
pub const REFRESH_TOKEN_FIELD: &str = "refresh_token";
/// The response field holding the access token a client sends to the provider.
pub const ACCESS_TOKEN_FIELD: &str = "access_token";
/// The request field naming the registered OAuth client, also the stored credential that keeps its id.
pub const CLIENT_ID_FIELD: &str = "client_id";
/// The authorization request's field naming where the provider returns the code, repeated on the code exchange.
pub const REDIRECT_URI_FIELD: &str = "redirect_uri";
/// The response field holding the access token's lifetime in seconds.
pub const EXPIRES_IN_FIELD: &str = "expires_in";
