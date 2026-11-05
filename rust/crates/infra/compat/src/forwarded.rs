//! The browser address a proxied request carries.
//!
//! The Tailscale Serve proxy sets `X-Forwarded-For` to the browser's address, and a request that
//! reaches the server directly carries no address. Both the request log and the diag telemetry
//! route read the address through this module, so one rule decides what a device is.

use http::{HeaderMap, HeaderName};

/// The header a Tailscale Serve proxy sets to the proxied request's client address
/// (`addProxyForwardedHeaders` in `ipn/ipnlocal/serve.go`).
pub const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The first `X-Forwarded-For` address, trimmed, or `-` when the request carries no address.
pub fn first_address(headers: &HeaderMap) -> String {
  headers
    .get(&X_FORWARDED_FOR)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.split(',').next())
    .map(str::trim)
    .filter(|address| !address.is_empty())
    .map_or_else(|| "-".to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
  use http::HeaderValue;

  use super::*;

  #[test]
  fn first_address_carries_the_proxy_header_and_falls_back_to_the_absent_marker() {
    let headers = |value: Option<&str>| {
      let mut headers = HeaderMap::new();
      if let Some(value) = value {
        headers.insert(&X_FORWARDED_FOR, HeaderValue::from_str(value).expect("the header value"));
      }
      headers
    };

    for (value, expected) in [
      (Some("100.64.0.8"), "100.64.0.8"),
      (Some("100.64.0.8, 10.1.2.3"), "100.64.0.8"),
      (Some(" 100.64.0.8 "), "100.64.0.8"),
      (None, "-"),
      (Some(""), "-"),
      (Some(","), "-"),
    ] {
      assert_eq!(first_address(&headers(value)), expected, "value: {value:?}");
    }
  }
}
