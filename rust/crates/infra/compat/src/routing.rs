//! The method answers of the Python app's routes. FastAPI registers each route with the methods it names and no
//! others, so a GET route does not answer HEAD. A request whose path matches a route with other methods gets
//! Starlette's 405: `{"detail": "Method Not Allowed"}` with an `allow` header.
//!
//! axum answers a HEAD request with the GET handler when the router names no HEAD handler. The helpers that serve GET
//! without serving HEAD name a HEAD handler that refuses, so the GET handler never answers HEAD by accident.

use std::future::{Ready, ready};

use axum::handler::Handler;
use axum::response::Response;
use axum::routing::{MethodRouter, get, post};

use crate::responses;

/// The handler that answers the 405 with `allow`. A method router takes it as its HEAD or fallback handler.
fn refuse(allow: &'static str) -> impl FnOnce() -> Ready<Response> + Clone + Send + Sync + 'static {
  move || ready(responses::method_not_allowed(allow))
}

/// A FastAPI GET route: GET is served. HEAD and every other method answer 405 with `allow: GET`.
pub fn get_only<H, T, S>(handler: H) -> MethodRouter<S>
where
  H: Handler<T, S>,
  T: 'static,
  S: Clone + Send + Sync + 'static,
{
  get(handler).head(refuse("GET")).fallback(refuse("GET"))
}

/// A route that FastAPI registers with `methods=["GET", "HEAD"]`: GET and HEAD are served, with HEAD answered by the
/// GET handler (hyper drops the body of a HEAD response). Every other method answers 405 with `allow: GET, HEAD`.
/// Starlette joins the methods of a set, whose order varies per process, so the value here is this one fixed order.
pub fn get_and_head<H, T, S>(handler: H) -> MethodRouter<S>
where
  H: Handler<T, S>,
  T: 'static,
  S: Clone + Send + Sync + 'static,
{
  get(handler).fallback(refuse("GET, HEAD"))
}

/// A FastAPI POST route: POST is served. HEAD and every other method answer 405 with `allow: POST`.
pub fn post_only<H, T, S>(handler: H) -> MethodRouter<S>
where
  H: Handler<T, S>,
  T: 'static,
  S: Clone + Send + Sync + 'static,
{
  post(handler).fallback(refuse("POST"))
}

/// A path that serves several methods, through `method_router`. Each method the router does not serve answers 405
/// with `allow`, and so does HEAD: this helper sets the HEAD handler, so `method_router` must not name one.
///
/// `allow` follows Starlette's rule: it lists the methods of the first route that Python registers on the path, the
/// first partial match for the request. Those methods can differ from the methods this router serves.
pub fn refuse_other_methods<S>(method_router: MethodRouter<S>, allow: &'static str) -> MethodRouter<S>
where
  S: Clone + Send + Sync + 'static,
{
  method_router.head(refuse(allow)).fallback(refuse(allow))
}

#[cfg(test)]
mod tests {
  use axum::Router;
  use axum::body::{Body, to_bytes};
  use axum::http::{Method, Request, StatusCode, header};
  use serde_json::{Value, json};
  use tower::ServiceExt;

  use super::*;

  async fn ok() -> &'static str {
    "ok"
  }

  async fn send(router: Router, method: Method) -> Response {
    router.oneshot(Request::builder().method(method).uri("/r").body(Body::empty()).unwrap()).await.unwrap()
  }

  async fn assert_served(router: Router, method: Method) {
    assert_eq!(send(router, method).await.status(), StatusCode::OK);
  }

  /// The FastAPI refusal: 405 and the `allow` header, then the `{"detail": ...}` body. axum drops the body of a HEAD
  /// response, as hyper does, so a HEAD refusal has none to check.
  async fn assert_refused(router: Router, method: Method, allow: &str) {
    let head = method == Method::HEAD;
    let response = send(router, method).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers()[header::ALLOW], allow);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    if !head {
      assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({"detail": "Method Not Allowed"}));
    }
  }

  #[tokio::test]
  async fn get_only_serves_get_and_refuses_head_and_other_methods() {
    let router = || Router::<()>::new().route("/r", get_only(ok));
    assert_served(router(), Method::GET).await;
    assert_refused(router(), Method::HEAD, "GET").await;
    assert_refused(router(), Method::PUT, "GET").await;
  }

  #[tokio::test]
  async fn get_and_head_serves_get_and_head_and_refuses_other_methods() {
    let router = || Router::<()>::new().route("/r", get_and_head(ok));
    assert_served(router(), Method::GET).await;
    assert_served(router(), Method::HEAD).await;
    assert_refused(router(), Method::POST, "GET, HEAD").await;
  }

  #[tokio::test]
  async fn post_only_serves_post_and_refuses_head_and_other_methods() {
    let router = || Router::<()>::new().route("/r", post_only(ok));
    assert_served(router(), Method::POST).await;
    assert_refused(router(), Method::HEAD, "POST").await;
    assert_refused(router(), Method::GET, "POST").await;
  }

  #[tokio::test]
  async fn refuse_other_methods_serves_its_methods_and_refuses_head_and_the_rest() {
    let router = || Router::<()>::new().route("/r", refuse_other_methods(get(ok).post(ok), "GET"));
    assert_served(router(), Method::GET).await;
    assert_served(router(), Method::POST).await;
    assert_refused(router(), Method::HEAD, "GET").await;
    assert_refused(router(), Method::PUT, "GET").await;
  }
}
