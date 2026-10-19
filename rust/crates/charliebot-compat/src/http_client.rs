//! The outbound HTTP client, as `src/infra/http.py` builds its `httpx.AsyncClient`.

use std::sync::Arc;

use reqwest::{Proxy, Url};

pub use crate::transport::{ClientError, Settings};

/// The outbound HTTP client with shared TLS roots and HTTPS proxy selection. A 3xx response is returned as it
/// arrived: `httpx.AsyncClient()` does not follow redirects by default, and no Python caller asks it to.
pub fn client(settings: &Settings) -> Result<reqwest::Client, ClientError> {
  let tls = crate::transport::tls_config(settings.cert_file.as_deref())?;
  let proxy_settings = crate::transport::proxy_settings(settings)?;
  let mut builder =
    reqwest::Client::builder().use_preconfigured_tls(tls).no_proxy().redirect(reqwest::redirect::Policy::none());
  if let Some(proxy_settings) = proxy_settings {
    let proxy_settings = Arc::new(proxy_settings);
    builder = builder.proxy(Proxy::custom(move |url: &Url| {
      proxy_settings.should_proxy_http_url(url).then(|| proxy_settings.url.clone())
    }));
  }
  Ok(builder.build()?)
}
