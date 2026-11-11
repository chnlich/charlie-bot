//! The HTTP header pieces that more than one crate spells: the names, and the bearer value an `Authorization` header
//! carries.
//!
//! A header name compares case-insensitively, but every sender writes the same spelling, so the name lives here once.
//! The writer of a bearer value and the reader that takes it back from a header share one prefix.

/// The `Authorization` header name. A request names its bearer credential in this header.
pub const AUTHORIZATION: &str = "Authorization";

/// The prefix of the bearer credential in an `Authorization` header value: `Bearer` and one space.
pub const BEARER_PREFIX: &str = "Bearer ";

/// The `Authorization` header value that carries `token` as a bearer credential.
pub fn bearer_value(token: &str) -> String {
  format!("{BEARER_PREFIX}{token}")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::run_token::bearer_from_authorization;

  #[test]
  fn the_bearer_value_is_the_wire_form_the_authorization_reader_takes_back() {
    assert_eq!(bearer_value("synthetic-token"), "Bearer synthetic-token");
    assert_eq!(bearer_from_authorization(Some(&bearer_value("synthetic-token"))), "synthetic-token");
  }
}
