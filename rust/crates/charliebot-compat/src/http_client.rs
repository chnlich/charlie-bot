//! The outbound HTTP clients, as the Python app builds them: `src/infra/http.py`'s `httpx.AsyncClient` and the
//! `urllib.request.urlopen` of the speech model download.

use std::sync::Arc;

use reqwest::{Proxy, Url};

pub use crate::transport::{ClientError, Settings};

/// The outbound HTTP client with shared TLS roots and HTTPS proxy selection. A 3xx response is returned as it
/// arrived: `httpx.AsyncClient()` does not follow redirects by default, and no Python caller asks it to.
pub fn client(settings: &Settings) -> Result<reqwest::Client, ClientError> {
  build_client(settings, reqwest::redirect::Policy::none())
}

/// The client for `urllib.request.urlopen` in `src/features/voice/transcriber.py:362`, the speech model download.
/// urlopen follows up to ten redirects (`HTTPRedirectHandler.max_redirections`), so this client follows as many.
/// The TLS roots and proxy selection are those of [`client`].
pub fn urllib_client(settings: &Settings) -> Result<reqwest::Client, ClientError> {
  build_client(settings, reqwest::redirect::Policy::limited(10))
}

fn build_client(settings: &Settings, redirect: reqwest::redirect::Policy) -> Result<reqwest::Client, ClientError> {
  let tls = crate::transport::tls_config(settings.cert_file.as_deref())?;
  let proxy_settings = crate::transport::proxy_settings(settings)?;
  let mut builder = reqwest::Client::builder().use_preconfigured_tls(tls).no_proxy().redirect(redirect);
  if let Some(proxy_settings) = proxy_settings {
    let proxy_settings = Arc::new(proxy_settings);
    builder = builder.proxy(Proxy::custom(move |url: &Url| {
      proxy_settings.should_proxy_http_url(url).then(|| proxy_settings.url.clone())
    }));
  }
  Ok(builder.build()?)
}
