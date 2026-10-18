//! The outbound HTTP client, as `src/infra/http.py` builds its `httpx.AsyncClient`: rustls with the
//! platform's root certificates, `SSL_CERT_FILE` in their place when it names a bundle, `HTTPS_PROXY` for
//! https requests, and `NO_PROXY` for the hosts that skip that proxy.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::{NoProxy, Proxy};
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;

/// The variables the client reads. An empty value counts as unset, as it does for httpx.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Settings {
  pub https_proxy: Option<String>,
  pub no_proxy: Option<String>,
  pub cert_file: Option<PathBuf>,
}

impl Settings {
  /// The settings the process environment names.
  pub fn from_env() -> Settings {
    Settings::from_lookup(|name| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()))
  }

  /// The settings `lookup` names: `from_env` with the source of the values made explicit.
  pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Settings {
    let value = |name: &str| lookup(name).filter(|value| !value.is_empty());
    Settings {
      https_proxy: value("HTTPS_PROXY"),
      no_proxy: value("NO_PROXY"),
      cert_file: value("SSL_CERT_FILE").map(PathBuf::from),
    }
  }
}

/// A client the settings cannot build: a certificate source with no usable certificate, or a proxy URL or
/// TLS configuration that reqwest rejects.
#[derive(Debug)]
pub enum ClientError {
  Tls(String),
  Reqwest(reqwest::Error),
}

impl fmt::Display for ClientError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ClientError::Tls(message) => f.write_str(message),
      ClientError::Reqwest(error) => write!(f, "{error}"),
    }
  }
}

impl std::error::Error for ClientError {}

impl From<reqwest::Error> for ClientError {
  fn from(error: reqwest::Error) -> Self {
    ClientError::Reqwest(error)
  }
}

/// The client the outbound requests share, built from `settings`.
pub fn client(settings: &Settings) -> Result<reqwest::Client, ClientError> {
  let tls = tls_config(settings.cert_file.as_deref())?;
  // reqwest would otherwise read HTTP_PROXY, ALL_PROXY and NO_PROXY from the environment itself; only
  // HTTPS_PROXY is honored here.
  let mut builder = reqwest::Client::builder().use_preconfigured_tls(tls).no_proxy();
  if let Some(url) = &settings.https_proxy {
    let no_proxy = settings.no_proxy.as_deref().and_then(NoProxy::from_string);
    builder = builder.proxy(Proxy::https(url.as_str())?.no_proxy(no_proxy));
  }
  Ok(builder.build()?)
}

/// The trust roots of `cert_file`'s bundle when it is set, else the platform's.
fn tls_config(cert_file: Option<&Path>) -> Result<ClientConfig, ClientError> {
  let mut roots = RootCertStore::empty();
  match cert_file {
    Some(path) => {
      let certs = CertificateDer::pem_file_iter(path)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|error| ClientError::Tls(format!("SSL_CERT_FILE {}: {error}", path.display())))?;
      if certs.is_empty() {
        return Err(ClientError::Tls(format!("SSL_CERT_FILE {} holds no certificate", path.display())));
      }
      for cert in certs {
        roots.add(cert).map_err(|error| ClientError::Tls(format!("SSL_CERT_FILE {}: {error}", path.display())))?;
      }
    }
    None => {
      let found = rustls_native_certs::load_native_certs();
      if found.certs.is_empty() {
        return Err(ClientError::Tls(format!("the platform holds no root certificate: {:?}", found.errors)));
      }
      for error in &found.errors {
        tracing::warn!(%error, "a platform root certificate failed to load");
      }
      let (_, ignored) = roots.add_parsable_certificates(found.certs);
      if ignored > 0 {
        tracing::warn!(ignored, "platform root certificates that rustls cannot parse were skipped");
      }
    }
  }
  let provider = Arc::new(rustls::crypto::ring::default_provider());
  let builder = ClientConfig::builder_with_provider(provider)
    .with_safe_default_protocol_versions()
    .map_err(|error| ClientError::Tls(error.to_string()))?;
  Ok(builder.with_root_certificates(roots).with_no_client_auth())
}
