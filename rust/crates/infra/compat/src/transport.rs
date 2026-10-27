//! Shared outbound transport settings, TLS roots, and HTTPS proxy selection.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hyper_util::client::proxy::matcher::Matcher;
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;

/// The variables the outbound clients read. Empty values count as unset, as they do for httpx.
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

/// A client the settings cannot build: a certificate source with no usable certificate, or a proxy URL or TLS
/// configuration that reqwest rejects.
#[derive(Debug)]
pub enum ClientError {
  Tls(String),
  Proxy(String),
  Reqwest(reqwest::Error),
}

impl fmt::Display for ClientError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ClientError::Tls(message) | ClientError::Proxy(message) => f.write_str(message),
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

/// A validated HTTPS proxy and its shared `NO_PROXY` matcher.
pub(crate) struct ProxySettings {
  pub(crate) url: reqwest::Url,
  matcher: Arc<Matcher>,
}

impl ProxySettings {
  pub(crate) fn should_proxy_http_url(&self, url: &reqwest::Url) -> bool {
    let uri = url.as_str().parse::<http::Uri>().expect("a reqwest URL is a valid HTTP URI");
    self.matcher.intercept(&uri).is_some()
  }

  pub(crate) fn should_proxy_wss_host(&self, host: &str, port: u16) -> bool {
    let authority_host =
      if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    let uri =
      format!("https://{authority_host}:{port}/").parse::<http::Uri>().expect("a WebSocket host is a URI authority");
    self.matcher.intercept(&uri).is_some()
  }
}

/// Build the selected proxy configuration once for an outbound client.
pub(crate) fn proxy_settings(settings: &Settings) -> Result<Option<ProxySettings>, ClientError> {
  let Some(raw_url) = settings.https_proxy.as_deref() else {
    return Ok(None);
  };
  let _validated = reqwest::Proxy::https(raw_url)?;
  let url = reqwest::Url::parse(raw_url).map_err(|error| ClientError::Proxy(error.to_string()))?;
  let matcher = Matcher::builder().https(raw_url).no(settings.no_proxy.as_deref().unwrap_or_default()).build();
  Ok(Some(ProxySettings { url, matcher: Arc::new(matcher) }))
}

/// The trust roots of `cert_file`'s bundle when it is set, else the platform's.
pub(crate) fn tls_config(cert_file: Option<&Path>) -> Result<ClientConfig, ClientError> {
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
