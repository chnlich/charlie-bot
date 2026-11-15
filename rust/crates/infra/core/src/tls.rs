//! The process's TLS crypto provider.

use std::sync::OnceLock;

/// Make a rustls crypto provider available to the process's plain `reqwest` clients.
///
/// The workspace's reqwest build carries no default provider, so its client builder refuses to build until one is
/// installed. A failed install means another provider already serves the process, which is the same outcome.
pub fn ensure_crypto_provider() {
  static INSTALLED: OnceLock<()> = OnceLock::new();
  INSTALLED.get_or_init(|| {
    let _ = rustls::crypto::ring::default_provider().install_default();
  });
}
