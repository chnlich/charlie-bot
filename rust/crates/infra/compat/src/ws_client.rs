//! Outbound WebSocket connections with the TLS roots and HTTPS proxy rules shared with `http_client`.
//!
//! The voice relay can forward the returned error text to the browser. These mappings match the `str()` output of the
//! corresponding websockets 16 exception:
//!
//! | Failure | Websockets 16 exception | Display text |
//! | --- | --- | --- |
//! | The server answers the opening request with a non-101 status | `InvalidStatus` | `server rejected WebSocket connection: HTTP {status}` |
//! | The proxy rejects `CONNECT` | `InvalidProxyStatus` | `proxy rejected connection: HTTP {status}` |
//! | The opening timeout expires during connect, `CONNECT`, TLS, or handshake | `TimeoutError` | `timed out during opening handshake` |
//! | A peer closes with a normal close frame | `ConnectionClosedOK` | `received {received}; then sent {sent}` or `sent {sent}; then received {received}` |
//! | A peer closes with an error close frame | `ConnectionClosedError` | `received {received}; then sent {sent}` or `sent {sent}; then received {received}` |
//! | An incoming message exceeds `max_size` | `ConnectionClosedError` | `sent 1009 (message too big) frame with {size} bytes exceeds limit of {max_size} bytes; no close frame received` |
//! | A keepalive ping receives no pong before `ping_timeout` | `ConnectionClosedError` | `sent 1011 (internal error) keepalive ping timeout; no close frame received` |
//!
//! Close-frame text includes its code and reason; `ConnectionClosedOK` and `ConnectionClosedError` have the same
//! base `ConnectionClosed` display format. TLS, socket, proxy-configuration, and other protocol errors retain their
//! Rust transport error text.
//!
//! The handshake omits the `User-Agent` header and doesn't offer `permessage-deflate`. The Python client sends both
//! by default, and the parity harness that ignored them on its fakes was removed on 2026-10-27.

use std::fmt;
use std::future::pending;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, Stream, StreamExt};
use http::Uri;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::{CapacityError, Error as TungsteniteError};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{
  CloseFrame as TungsteniteCloseFrame, Message as TungsteniteMessage, WebSocketConfig,
};
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

pub use crate::http_client::Settings;

/// The websockets 16 client defaults used by Python call sites that omit an option.
pub const WEBSOCKETS_16_DEFAULTS: Options = Options {
  open_timeout: Some(Duration::from_secs(10)),
  close_timeout: Some(Duration::from_secs(10)),
  max_size: Some(1_048_576),
  ping_interval: Some(Duration::from_secs(20)),
  ping_timeout: Some(Duration::from_secs(20)),
};

/// Connection timeouts, incoming-message limit, and keepalive settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
  pub open_timeout: Option<Duration>,
  pub close_timeout: Option<Duration>,
  pub max_size: Option<usize>,
  pub ping_interval: Option<Duration>,
  pub ping_timeout: Option<Duration>,
}

/// A WebSocket close frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseFrame {
  pub code: u16,
  pub reason: String,
}

/// The data messages the sender and receiver exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
  Text(String),
  Binary(Vec<u8>),
  Close(Option<CloseFrame>),
}

/// A WebSocket connection failure, displayed with websockets 16 text for matching failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
  InvalidUri(String, String),
  InvalidProxy(String, String),
  InvalidProxyMessage(String),
  InvalidProxyStatus(u16),
  InvalidStatus(u16),
  OpenTimeout,
  Settings(String),
  Tls(String),
  Io(String),
  Protocol(String),
  ConnectionClosed { received: Option<CloseFrame>, sent: Option<CloseFrame>, received_then_sent: Option<bool> },
}

impl fmt::Display for Error {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Error::InvalidUri(uri, message) => write!(formatter, "{uri} isn't a valid URI: {message}"),
      Error::InvalidProxy(proxy, message) => write!(formatter, "{proxy} isn't a valid proxy: {message}"),
      Error::InvalidProxyMessage(message) => formatter.write_str(message),
      Error::InvalidProxyStatus(status) => write!(formatter, "proxy rejected connection: HTTP {status}"),
      Error::InvalidStatus(status) => write!(formatter, "server rejected WebSocket connection: HTTP {status}"),
      Error::OpenTimeout => formatter.write_str("timed out during opening handshake"),
      Error::Settings(message) | Error::Tls(message) | Error::Io(message) | Error::Protocol(message) => {
        formatter.write_str(message)
      }
      Error::ConnectionClosed { received, sent, received_then_sent } => match (received, sent) {
        (None, None) => formatter.write_str("no close frame received or sent"),
        (None, Some(sent)) => write!(formatter, "sent {sent}; no close frame received"),
        (Some(received), None) => write!(formatter, "received {received}; no close frame sent"),
        (Some(received), Some(sent)) if *received_then_sent == Some(true) => {
          write!(formatter, "received {received}; then sent {sent}")
        }
        (Some(received), Some(sent)) => write!(formatter, "sent {sent}; then received {received}"),
      },
    }
  }
}

impl std::error::Error for Error {}

impl fmt::Display for CloseFrame {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let explanation = match self.code {
      1000 => "OK",
      1001 => "going away",
      1002 => "protocol error",
      1003 => "unsupported data",
      1005 => "no status received [internal]",
      1006 => "abnormal closure [internal]",
      1007 => "invalid frame payload data",
      1008 => "policy violation",
      1009 => "message too big",
      1010 => "mandatory extension",
      1011 => "internal error",
      1012 => "service restart",
      1013 => "try again later",
      1014 => "bad gateway",
      3000..=3999 => "registered",
      4000..=4999 => "private use",
      _ => "unknown",
    };
    write!(formatter, "{} ({explanation})", self.code)?;
    if !self.reason.is_empty() {
      write!(formatter, " {}", self.reason)?;
    }
    Ok(())
  }
}

/// The sending half of a WebSocket connection.
pub struct Sender {
  commands: mpsc::UnboundedSender<Command>,
  abort: AbortHandle,
}

impl Sender {
  /// Send a text or binary message. Sending a close message performs a graceful close.
  pub async fn send(&self, message: Message) -> Result<(), Error> {
    match message {
      Message::Close(frame) => self.close_with(frame).await,
      message => {
        let (response, result) = oneshot::channel();
        self.commands.send(Command::Send(message, response)).map_err(|_| connection_closed(None, None, None))?;
        result.await.map_err(|_| connection_closed(None, None, None))?
      }
    }
  }

  /// Send a normal close frame and wait up to `close_timeout` for the peer's reply.
  pub async fn close(&self) -> Result<(), Error> {
    self.close_with(None).await
  }

  async fn close_with(&self, frame: Option<CloseFrame>) -> Result<(), Error> {
    let (response, result) = oneshot::channel();
    self.commands.send(Command::Close(frame, response)).map_err(|_| connection_closed(None, None, None))?;
    result.await.map_err(|_| connection_closed(None, None, None))?
  }

  /// Drop the transport immediately without a closing handshake.
  pub fn abort(&self) {
    self.abort.abort();
  }
}

impl Drop for Sender {
  fn drop(&mut self) {
    self.abort.abort();
  }
}

/// The receiving half of a WebSocket connection.
pub struct Receiver {
  messages: mpsc::Receiver<Result<Message, Error>>,
}

impl Receiver {
  /// Receive the next text, binary, or close message, or its connection error.
  pub async fn recv(&mut self) -> Option<Result<Message, Error>> {
    self.messages.recv().await
  }
}

impl Stream for Receiver {
  type Item = Result<Message, Error>;

  fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    self.messages.poll_recv(context)
  }
}

/// Connect and complete the WebSocket opening handshake.
///
/// The open timeout covers TCP connect, an optional HTTP CONNECT tunnel, TLS, and the WebSocket handshake. `ws`
/// URLs connect directly. `wss` URLs use `HTTPS_PROXY` unless `NO_PROXY` matches the target host.
pub async fn connect(url: &str, options: &Options, settings: &Settings) -> Result<(Sender, Receiver), Error> {
  let target = reqwest::Url::parse(url).map_err(|error| Error::InvalidUri(url.to_string(), error.to_string()))?;
  if target.scheme() != "ws" && target.scheme() != "wss" {
    return Err(Error::InvalidUri(url.to_string(), "scheme isn't ws or wss".to_string()));
  }
  let host = target
    .host_str()
    .ok_or_else(|| Error::InvalidUri(url.to_string(), "hostname isn't provided".to_string()))?
    .to_string();
  let port = target
    .port_or_known_default()
    .ok_or_else(|| Error::InvalidUri(url.to_string(), "port isn't provided".to_string()))?;
  let proxy = if target.scheme() == "wss" {
    crate::transport::proxy_settings(settings)
      .map_err(|error| Error::Settings(error.to_string()))?
      .filter(|proxy| proxy.should_proxy_wss_host(&host, port))
  } else {
    None
  };
  let open = connect_opening_handshake(target, host, port, proxy, options.max_size, settings);
  let (socket, _) = match options.open_timeout {
    Some(open_timeout) => timeout(open_timeout, open).await.map_err(|_| Error::OpenTimeout)??,
    None => open.await?,
  };
  let (commands, command_receiver) = mpsc::unbounded_channel();
  let (message_sender, messages) = mpsc::channel(16);
  let driver = tokio::spawn(drive(socket, *options, command_receiver, message_sender));
  let abort = driver.abort_handle();
  drop(driver);
  Ok((Sender { commands, abort }, Receiver { messages }))
}

type WebSocket = WebSocketStream<Box<dyn AsyncIo>>;

trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIo for T {}

async fn connect_opening_handshake(
  target: reqwest::Url,
  host: String,
  port: u16,
  proxy: Option<crate::transport::ProxySettings>,
  max_size: Option<usize>,
  settings: &Settings,
) -> Result<(WebSocket, http::Response<Option<Vec<u8>>>), Error> {
  let tls = if target.scheme() == "wss" || proxy.as_ref().is_some_and(|proxy| proxy.url.scheme() == "https") {
    Some(TlsConnector::from(Arc::new(
      crate::transport::tls_config(settings.cert_file.as_deref())
        .map_err(|error| Error::Settings(error.to_string()))?,
    )))
  } else {
    None
  };
  let stream = match proxy {
    Some(proxy) => connect_via_proxy(proxy, &target, &host, port, tls.as_ref()).await?,
    None => connect_direct(&host, port, target.scheme() == "wss", tls.as_ref()).await?,
  };
  let request = target
    .as_str()
    .parse::<Uri>()
    .map_err(|error| Error::InvalidUri(target.to_string(), error.to_string()))?
    .into_client_request()
    .map_err(map_tungstenite_error)?;
  let config = WebSocketConfig::default().max_message_size(max_size).max_frame_size(max_size);
  client_async_with_config(request, stream, Some(config)).await.map_err(map_tungstenite_error)
}

async fn connect_direct(
  host: &str,
  port: u16,
  secure: bool,
  tls: Option<&TlsConnector>,
) -> Result<Box<dyn AsyncIo>, Error> {
  let stream = TcpStream::connect((unbracket(host), port)).await.map_err(|error| Error::Io(error.to_string()))?;
  if secure {
    let connector = tls.expect("secure WebSocket URLs have a TLS connector");
    tls_connect(connector, host, Box::new(stream)).await
  } else {
    Ok(Box::new(stream))
  }
}

async fn connect_via_proxy(
  proxy: crate::transport::ProxySettings,
  target: &reqwest::Url,
  host: &str,
  port: u16,
  tls: Option<&TlsConnector>,
) -> Result<Box<dyn AsyncIo>, Error> {
  let proxy_url = &proxy.url;
  let proxy_host = proxy_url
    .host_str()
    .ok_or_else(|| Error::InvalidProxy(proxy_url.to_string(), "hostname isn't provided".to_string()))?;
  let proxy_port = proxy_url
    .port_or_known_default()
    .ok_or_else(|| Error::InvalidProxy(proxy_url.to_string(), "port isn't provided".to_string()))?;
  if !matches!(proxy_url.scheme(), "http" | "https") {
    return Err(Error::InvalidProxy(proxy_url.to_string(), format!("scheme {} isn't supported", proxy_url.scheme())));
  }
  if !matches!(proxy_url.path(), "" | "/") {
    return Err(Error::InvalidProxy(proxy_url.to_string(), "path is meaningless".to_string()));
  }
  if proxy_url.query().is_some() {
    return Err(Error::InvalidProxy(proxy_url.to_string(), "query is meaningless".to_string()));
  }
  if proxy_url.fragment().is_some() {
    return Err(Error::InvalidProxy(proxy_url.to_string(), "fragment is meaningless".to_string()));
  }
  if !proxy_url.username().is_empty() && proxy_url.password().is_none() {
    return Err(Error::InvalidProxy(proxy_url.to_string(), "username provided without password".to_string()));
  }
  let stream =
    TcpStream::connect((unbracket(proxy_host), proxy_port)).await.map_err(|error| Error::Io(error.to_string()))?;
  let mut stream: Box<dyn AsyncIo> = if proxy_url.scheme() == "https" {
    tls_connect(tls.expect("secure proxy URLs have a TLS connector"), proxy_host, Box::new(stream)).await?
  } else {
    Box::new(stream)
  };
  connect_tunnel(&mut stream, proxy_url, host, port, target.scheme() == "wss").await?;
  tls_connect(tls.expect("wss proxy tunnels have a TLS connector"), host, stream).await
}

async fn connect_tunnel(
  stream: &mut Box<dyn AsyncIo>,
  proxy: &reqwest::Url,
  host: &str,
  port: u16,
  secure: bool,
) -> Result<(), Error> {
  let authority = authority(host, port);
  let default_port = if secure { 443 } else { 80 };
  let host_header = if port == default_port { authority_host(host) } else { authority.clone() };
  let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {host_header}\r\n");
  if !proxy.username().is_empty() {
    let credentials = format!("{}:{}", proxy.username(), proxy.password().unwrap_or_default());
    let authorization = base64::engine::general_purpose::STANDARD.encode(credentials);
    request.push_str(&format!("Proxy-Authorization: Basic {authorization}\r\n"));
  }
  request.push_str("\r\n");
  stream.write_all(request.as_bytes()).await.map_err(|error| Error::Io(error.to_string()))?;
  stream.flush().await.map_err(|error| Error::Io(error.to_string()))?;
  let mut reader = BufReader::new(stream);
  let mut status_line = String::new();
  reader
    .read_line(&mut status_line)
    .await
    .map_err(|_| Error::InvalidProxyMessage("did not receive a valid HTTP response from proxy".to_string()))?;
  let status = parse_http_status(&status_line)
    .ok_or_else(|| Error::InvalidProxyMessage("did not receive a valid HTTP response from proxy".to_string()))?;
  loop {
    let mut line = String::new();
    reader
      .read_line(&mut line)
      .await
      .map_err(|_| Error::InvalidProxyMessage("did not receive a valid HTTP response from proxy".to_string()))?;
    if line.is_empty() {
      return Err(Error::InvalidProxyMessage("did not receive a valid HTTP response from proxy".to_string()));
    }
    if line == "\r\n" || line == "\n" {
      break;
    }
  }
  if !(200..300).contains(&status) {
    return Err(Error::InvalidProxyStatus(status));
  }
  Ok(())
}

async fn tls_connect(
  connector: &TlsConnector,
  host: &str,
  stream: Box<dyn AsyncIo>,
) -> Result<Box<dyn AsyncIo>, Error> {
  let server_name = ServerName::try_from(unbracket(host).to_string()).map_err(|error| Error::Tls(error.to_string()))?;
  let stream = connector.connect(server_name, stream).await.map_err(|error| Error::Tls(error.to_string()))?;
  Ok(Box::new(stream))
}

fn parse_http_status(line: &str) -> Option<u16> {
  let mut fields = line.split_ascii_whitespace();
  if !fields.next()?.starts_with("HTTP/") {
    return None;
  }
  fields.next()?.parse().ok()
}

fn authority(host: &str, port: u16) -> String {
  format!("{}:{port}", authority_host(host))
}

fn authority_host(host: &str) -> String {
  if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() }
}

fn unbracket(host: &str) -> &str {
  host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host)
}

fn map_tungstenite_error(error: TungsteniteError) -> Error {
  match error {
    TungsteniteError::Http(response) => Error::InvalidStatus(response.status().as_u16()),
    TungsteniteError::Tls(error) => Error::Tls(error.to_string()),
    TungsteniteError::Io(error) => Error::Io(error.to_string()),
    TungsteniteError::ConnectionClosed | TungsteniteError::AlreadyClosed => connection_closed(None, None, None),
    error => Error::Protocol(error.to_string()),
  }
}

enum Command {
  Send(Message, oneshot::Sender<Result<(), Error>>),
  Close(Option<CloseFrame>, oneshot::Sender<Result<(), Error>>),
}

#[derive(Default)]
struct CloseState {
  received: Option<CloseFrame>,
  sent: Option<CloseFrame>,
  received_then_sent: Option<bool>,
  deadline: Option<Instant>,
  waiters: Vec<oneshot::Sender<Result<(), Error>>>,
}

struct KeepaliveState {
  next_ping: Option<Instant>,
  timeout_deadline: Option<Instant>,
  next_payload: u64,
  pending_pong: Option<Vec<u8>>,
}

async fn drive(
  mut socket: WebSocket,
  options: Options,
  mut commands: mpsc::UnboundedReceiver<Command>,
  messages: mpsc::Sender<Result<Message, Error>>,
) {
  let mut close = CloseState::default();
  let mut keepalive = KeepaliveState {
    next_ping: options.ping_interval.map(|interval| Instant::now() + interval),
    timeout_deadline: None,
    next_payload: 1,
    pending_pong: None,
  };

  loop {
    tokio::select! {
      incoming = socket.next() => {
        match incoming {
          Some(Ok(TungsteniteMessage::Text(text))) => {
            if messages.send(Ok(Message::Text(text.to_string()))).await.is_err() {
              return;
            }
          }
          Some(Ok(TungsteniteMessage::Binary(data))) => {
            if messages.send(Ok(Message::Binary(data.to_vec()))).await.is_err() {
              return;
            }
          }
          Some(Ok(TungsteniteMessage::Ping(_))) => {
            if let Err(error) = socket.flush().await {
              finish(&messages, &mut close.waiters, map_tungstenite_error(error)).await;
              return;
            }
          }
          Some(Ok(TungsteniteMessage::Pong(data))) => {
            if keepalive.pending_pong.as_deref() == Some(data.as_ref()) {
              keepalive.pending_pong = None;
              keepalive.timeout_deadline = None;
              keepalive.next_ping = options.ping_interval.map(|interval| Instant::now() + interval);
            }
          }
          Some(Ok(TungsteniteMessage::Close(frame))) => {
            close.received = Some(frame.map(from_tungstenite_close).unwrap_or(CloseFrame {
              code: 1005,
              reason: String::new(),
            }));
            if close.sent.is_none() {
              close.sent = close.received.clone();
              close.received_then_sent = Some(true);
            } else {
              close.received_then_sent = Some(false);
            }
            if let Err(error) = socket.flush().await {
              finish(&messages, &mut close.waiters, map_tungstenite_error(error)).await;
              return;
            }
            for waiter in close.waiters.drain(..) {
              let _ = waiter.send(Ok(()));
            }
            publish_close(&messages, close.received.clone(), close.sent.clone(), close.received_then_sent).await;
            return;
          }
          Some(Ok(TungsteniteMessage::Frame(_))) => {}
          Some(Err(TungsteniteError::Capacity(CapacityError::MessageTooLong { size, max_size }))) => {
            let reason = format!("frame with {size} bytes exceeds limit of {max_size} bytes");
            let frame = CloseFrame { code: 1009, reason };
            if let Err(error) = socket.send(TungsteniteMessage::Close(Some(to_tungstenite_close(&frame)))).await {
              finish(&messages, &mut close.waiters, map_tungstenite_error(error)).await;
              return;
            }
            close.sent = Some(frame);
            publish_close(&messages, close.received, close.sent, close.received_then_sent).await;
            return;
          }
          Some(Err(TungsteniteError::ConnectionClosed | TungsteniteError::AlreadyClosed)) | None => {
            let error = connection_closed(close.received, close.sent, close.received_then_sent);
            for waiter in close.waiters.drain(..) {
              let _ = waiter.send(Ok(()));
            }
            let _ = messages.send(Err(error)).await;
            return;
          }
          Some(Err(error)) => {
            finish(&messages, &mut close.waiters, map_tungstenite_error(error)).await;
            return;
          }
        }
      }
      command = commands.recv() => {
        match command {
          Some(Command::Send(Message::Text(text), response)) => {
            let result = socket.send(TungsteniteMessage::Text(text.into())).await.map_err(map_tungstenite_error);
            let failed = result.as_ref().err().cloned();
            let _ = response.send(result);
            if let Some(error) = failed {
              let _ = messages.send(Err(error)).await;
              return;
            }
          }
          Some(Command::Send(Message::Binary(data), response)) => {
            let result = socket.send(TungsteniteMessage::Binary(data.into())).await.map_err(map_tungstenite_error);
            let failed = result.as_ref().err().cloned();
            let _ = response.send(result);
            if let Some(error) = failed {
              let _ = messages.send(Err(error)).await;
              return;
            }
          }
          Some(Command::Send(Message::Close(frame), response)) => {
            begin_close(&mut socket, &options, frame, response, &mut close, &mut keepalive).await;
          }
          Some(Command::Close(frame, response)) => {
            begin_close(&mut socket, &options, frame, response, &mut close, &mut keepalive).await;
          }
          None => return,
        }
      }
      _ = async {
        if let Some(deadline) = close.deadline {
          sleep_until(deadline).await;
        } else {
          pending::<()>().await;
        }
      }, if close.deadline.is_some() => {
        for waiter in close.waiters.drain(..) {
          let _ = waiter.send(Ok(()));
        }
        let error = connection_closed(close.received.clone(), close.sent.clone(), close.received_then_sent);
        let _ = messages.try_send(Err(error));
        return;
      }
      _ = async {
        if let Some(deadline) = keepalive.timeout_deadline {
          sleep_until(deadline).await;
        } else {
          pending::<()>().await;
        }
      }, if keepalive.timeout_deadline.is_some() => {
        let frame = CloseFrame { code: 1011, reason: "keepalive ping timeout".to_string() };
        if let Err(error) = socket.send(TungsteniteMessage::Close(Some(to_tungstenite_close(&frame)))).await {
          finish(&messages, &mut close.waiters, map_tungstenite_error(error)).await;
          return;
        }
        close.sent = Some(frame);
        publish_close(&messages, close.received, close.sent, close.received_then_sent).await;
        return;
      }
      _ = async {
        if let Some(deadline) = keepalive.next_ping {
          sleep_until(deadline).await;
        } else {
          pending::<()>().await;
        }
      }, if keepalive.next_ping.is_some() && keepalive.pending_pong.is_none() && close.deadline.is_none() => {
        let payload = keepalive.next_payload.to_be_bytes().to_vec();
        keepalive.next_payload = keepalive.next_payload.wrapping_add(1);
        if let Err(error) = socket.send(TungsteniteMessage::Ping(payload.clone().into())).await {
          finish(&messages, &mut close.waiters, map_tungstenite_error(error)).await;
          return;
        }
        keepalive.pending_pong = Some(payload);
        keepalive.timeout_deadline = options.ping_timeout.map(|duration| Instant::now() + duration);
        keepalive.next_ping = None;
      }
    }
  }
}

async fn begin_close(
  socket: &mut WebSocket,
  options: &Options,
  frame: Option<CloseFrame>,
  response: oneshot::Sender<Result<(), Error>>,
  close: &mut CloseState,
  keepalive: &mut KeepaliveState,
) {
  if close.sent.is_none() {
    let frame = frame.unwrap_or(CloseFrame { code: 1000, reason: String::new() });
    if close.received_then_sent.is_none() {
      close.received_then_sent = Some(false);
    }
    if let Err(error) = socket.send(TungsteniteMessage::Close(Some(to_tungstenite_close(&frame)))).await {
      let error = map_tungstenite_error(error);
      let _ = response.send(Err(error));
      return;
    }
    close.sent = Some(frame);
    close.deadline = options.close_timeout.map(|duration| Instant::now() + duration);
    keepalive.next_ping = None;
    keepalive.timeout_deadline = None;
  }
  close.waiters.push(response);
}

async fn finish(
  messages: &mpsc::Sender<Result<Message, Error>>,
  waiters: &mut Vec<oneshot::Sender<Result<(), Error>>>,
  error: Error,
) {
  for waiter in waiters.drain(..) {
    let _ = waiter.send(Err(error.clone()));
  }
  let _ = messages.send(Err(error)).await;
}

async fn publish_close(
  messages: &mpsc::Sender<Result<Message, Error>>,
  received: Option<CloseFrame>,
  sent: Option<CloseFrame>,
  received_then_sent: Option<bool>,
) {
  if let Some(received) = &received {
    let _ = messages.send(Ok(Message::Close(Some(received.clone())))).await;
  }
  let _ = messages.send(Err(connection_closed(received, sent, received_then_sent))).await;
}

fn connection_closed(
  received: Option<CloseFrame>,
  sent: Option<CloseFrame>,
  received_then_sent: Option<bool>,
) -> Error {
  Error::ConnectionClosed { received, sent, received_then_sent }
}

fn from_tungstenite_close(frame: TungsteniteCloseFrame) -> CloseFrame {
  CloseFrame { code: u16::from(frame.code), reason: frame.reason.to_string() }
}

fn to_tungstenite_close(frame: &CloseFrame) -> TungsteniteCloseFrame {
  TungsteniteCloseFrame { code: CloseCode::from(frame.code), reason: frame.reason.clone().into() }
}

#[cfg(test)]
mod tests {
  use std::net::SocketAddr;
  use std::sync::Arc;

  use base64::Engine;
  use futures_util::{SinkExt, StreamExt};
  use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair};
  use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
  use sha1::{Digest, Sha1};
  use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
  use tokio::net::{TcpListener, TcpStream};
  use tokio::time::{Duration, timeout};
  use tokio_rustls::TlsAcceptor;
  use tokio_tungstenite::accept_async;
  use tokio_tungstenite::tungstenite::Message as ServerMessage;

  use super::*;

  struct TestTls {
    acceptor: TlsAcceptor,
    settings: Settings,
    _directory: tempfile::TempDir,
  }

  fn test_tls() -> TestTls {
    let directory = tempfile::tempdir().expect("create synthetic TLS material");
    let ca_key = KeyPair::generate().expect("create throwaway CA key");
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).expect("create throwaway CA certificate");
    let leaf_key = KeyPair::generate().expect("create throwaway server key");
    let mut leaf_params =
      CertificateParams::new(vec!["localhost".to_string()]).expect("create server certificate parameters");
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).expect("sign the server certificate");
    let ca_file = directory.path().join("ca.pem");
    std::fs::write(&ca_file, ca.pem()).expect("write the throwaway CA bundle");
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
      .with_safe_default_protocol_versions()
      .expect("configure TLS protocol versions")
      .with_no_client_auth()
      .with_single_cert(vec![leaf.der().clone()], key)
      .expect("configure the server certificate");
    let settings =
      Settings::from_lookup(|name| (name == "SSL_CERT_FILE").then(|| ca_file.to_string_lossy().into_owned()));
    TestTls { acceptor: TlsAcceptor::from(Arc::new(config)), settings, _directory: directory }
  }

  fn options(
    max_size: Option<usize>,
    close_timeout: Duration,
    ping_interval: Option<Duration>,
    ping_timeout: Option<Duration>,
  ) -> Options {
    Options {
      open_timeout: Some(Duration::from_secs(2)),
      close_timeout: Some(close_timeout),
      max_size,
      ping_interval,
      ping_timeout,
    }
  }

  async fn tls_echo_server(listener: TcpListener, acceptor: TlsAcceptor) {
    let (stream, _) = listener.accept().await.expect("accept the TLS client");
    let stream = acceptor.accept(stream).await.expect("accept target TLS");
    let mut socket = accept_async(stream).await.expect("accept the WebSocket");
    while let Some(message) = socket.next().await {
      match message.expect("read a WebSocket message") {
        ServerMessage::Text(text) => socket.send(ServerMessage::Text(text)).await.expect("echo the text"),
        ServerMessage::Close(_) => {
          socket.flush().await.expect("flush the close response");
          return;
        }
        _ => {}
      }
    }
  }

  async fn proxy_tunnel(listener: TcpListener, target: SocketAddr) -> String {
    let (stream, _) = listener.accept().await.expect("accept the proxy connection");
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await.expect("read CONNECT");
    loop {
      let mut header = String::new();
      reader.read_line(&mut header).await.expect("read CONNECT headers");
      if header == "\r\n" || header == "\n" {
        break;
      }
    }
    let mut client = reader.into_inner();
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.expect("accept the tunnel");
    let mut upstream = TcpStream::connect(target).await.expect("connect the TLS target");
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await.expect("relay the TLS tunnel");
    request_line
  }

  async fn raw_websocket(stream: TcpStream) -> BufReader<TcpStream> {
    let mut reader = BufReader::new(stream);
    let mut key = String::new();
    let mut user_agent_present = false;
    let mut extensions_present = false;
    loop {
      let mut line = String::new();
      reader.read_line(&mut line).await.expect("read the opening request");
      if line.is_empty() {
        panic!("client closed during the opening request");
      }
      if let Some(value) = line.strip_prefix("Sec-WebSocket-Key:").or_else(|| line.strip_prefix("sec-websocket-key:")) {
        key = value.trim().to_string();
      }
      user_agent_present |= line.to_ascii_lowercase().starts_with("user-agent:");
      extensions_present |= line.to_ascii_lowercase().starts_with("sec-websocket-extensions:");
      if line == "\r\n" || line == "\n" {
        break;
      }
    }
    assert!(!key.is_empty(), "the request includes a WebSocket key");
    assert!(!user_agent_present, "the request omits User-Agent");
    assert!(!extensions_present, "the request omits permessage-deflate offers");
    let digest = Sha1::digest(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes());
    let accept = base64::engine::general_purpose::STANDARD.encode(digest);
    reader
      .get_mut()
      .write_all(
        format!(
          "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .as_bytes(),
      )
      .await
      .expect("write the WebSocket response");
    reader
  }

  struct RawFrame {
    opcode: u8,
    payload: Vec<u8>,
  }

  async fn read_raw_frame(reader: &mut BufReader<TcpStream>) -> RawFrame {
    let mut header = [0; 2];
    reader.read_exact(&mut header).await.expect("read a frame header");
    let masked = header[1] & 0x80 != 0;
    let mut length = u64::from(header[1] & 0x7f);
    if length == 126 {
      let mut extended = [0; 2];
      reader.read_exact(&mut extended).await.expect("read the 16-bit frame length");
      length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
      let mut extended = [0; 8];
      reader.read_exact(&mut extended).await.expect("read the 64-bit frame length");
      length = u64::from_be_bytes(extended);
    }
    let mut mask = [0; 4];
    if masked {
      reader.read_exact(&mut mask).await.expect("read the masking key");
    }
    let mut payload = vec![0; usize::try_from(length).expect("frame length fits in memory")];
    reader.read_exact(&mut payload).await.expect("read the frame payload");
    if masked {
      for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % mask.len()];
      }
    }
    RawFrame { opcode: header[0] & 0x0f, payload }
  }

  async fn write_raw_text(reader: &mut BufReader<TcpStream>, text: &[u8]) {
    assert!(text.len() < 126);
    let mut frame = vec![0x81, text.len() as u8];
    frame.extend_from_slice(text);
    reader.get_mut().write_all(&frame).await.expect("send a server text frame");
  }

  fn close_code(frame: &RawFrame) -> u16 {
    assert_eq!(frame.opcode, 8);
    u16::from_be_bytes([frame.payload[0], frame.payload[1]])
  }

  #[tokio::test]
  async fn wss_uses_connect_proxy_and_shared_custom_ca() {
    let tls = test_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the TLS target");
    let target = listener.local_addr().expect("read target address");
    let server = tokio::spawn(tls_echo_server(listener, tls.acceptor));
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the CONNECT proxy");
    let proxy_address = proxy_listener.local_addr().expect("read proxy address");
    let proxy = tokio::spawn(proxy_tunnel(proxy_listener, target));
    let proxy_url = format!("http://{proxy_address}");
    let settings = Settings::from_lookup(|name| match name {
      "HTTPS_PROXY" => Some(proxy_url.clone()),
      "NO_PROXY" => Some("bypass.invalid".to_string()),
      "SSL_CERT_FILE" => tls.settings.cert_file.as_ref().map(|path| path.to_string_lossy().into_owned()),
      _ => None,
    });
    let url = format!("wss://localhost:{}/socket", target.port());
    let options = options(Some(1024), Duration::from_secs(1), None, None);
    let (sender, mut receiver) = connect(&url, &options, &settings).await.expect("connect through the proxy");
    timeout(Duration::from_secs(2), sender.send(Message::Text("synthetic message".to_string())))
      .await
      .expect("send completes")
      .expect("send text");
    assert_eq!(
      timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("echo arrives")
        .expect("receive the echo")
        .expect("text succeeds"),
      Message::Text("synthetic message".to_string())
    );
    timeout(Duration::from_secs(2), sender.close())
      .await
      .expect("close completes")
      .expect("complete the close handshake");
    assert!(matches!(
      timeout(Duration::from_secs(2), receiver.recv()).await.expect("close frame arrives"),
      Some(Ok(Message::Close(_)))
    ));
    let close_error = timeout(Duration::from_secs(2), receiver.recv())
      .await
      .expect("close error arrives")
      .expect("receive the close error")
      .expect_err("close ends the stream");
    assert!(close_error.to_string().contains("1000 (OK)"));
    let request =
      timeout(Duration::from_secs(2), proxy).await.expect("proxy task finishes").expect("proxy task succeeds");
    assert_eq!(request, format!("CONNECT localhost:{} HTTP/1.1\r\n", target.port()));
    timeout(Duration::from_secs(2), server).await.expect("TLS server finishes").expect("TLS server succeeds");
  }

  #[tokio::test]
  async fn non_switching_status_uses_websockets_invalid_status_text() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the rejecting peer");
    let address = listener.local_addr().expect("read peer address");
    let server = tokio::spawn(async move {
      let (stream, _) = listener.accept().await.expect("accept the client");
      let mut reader = BufReader::new(stream);
      loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read the opening request");
        if line == "\r\n" || line == "\n" {
          break;
        }
      }
      reader
        .get_mut()
        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
        .await
        .expect("reject the WebSocket request");
    });
    let url = format!("ws://{address}/socket");
    let error =
      match connect(&url, &options(Some(1024), Duration::from_secs(1), None, None), &Settings::default()).await {
        Err(error) => error,
        Ok(_) => panic!("HTTP 403 must reject the WebSocket opening handshake"),
      };
    assert_eq!(error.to_string(), "server rejected WebSocket connection: HTTP 403");
    timeout(Duration::from_secs(2), server).await.expect("rejecting server finishes").expect("server succeeds");
  }

  #[tokio::test]
  async fn proxy_refusal_uses_websockets_invalid_proxy_status_text() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the rejecting proxy");
    let address = listener.local_addr().expect("read proxy address");
    let server = tokio::spawn(async move {
      let (stream, _) = listener.accept().await.expect("accept the proxy client");
      let mut reader = BufReader::new(stream);
      loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read CONNECT");
        if line == "\r\n" || line == "\n" {
          break;
        }
      }
      reader
        .get_mut()
        .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
        .await
        .expect("reject CONNECT");
    });
    let proxy_url = format!("http://{address}");
    let settings = Settings::from_lookup(|name| (name == "HTTPS_PROXY").then(|| proxy_url.clone()));
    let error =
      match connect("wss://target.invalid/socket", &options(Some(1024), Duration::from_secs(1), None, None), &settings)
        .await
      {
        Err(error) => error,
        Ok(_) => panic!("HTTP 407 must reject the proxy tunnel"),
      };
    assert_eq!(error.to_string(), "proxy rejected connection: HTTP 407");
    timeout(Duration::from_secs(2), server).await.expect("rejecting proxy finishes").expect("server succeeds");
  }

  #[tokio::test]
  async fn no_proxy_host_connects_directly() {
    let tls = test_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the TLS target");
    let target = listener.local_addr().expect("read target address");
    let server = tokio::spawn(tls_echo_server(listener, tls.acceptor));
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the unused proxy");
    let proxy_address = proxy_listener.local_addr().expect("read unused proxy address");
    let proxy_watch = tokio::spawn(async move { timeout(Duration::from_millis(200), proxy_listener.accept()).await });
    let settings = Settings::from_lookup(|name| match name {
      "HTTPS_PROXY" => Some(format!("http://{proxy_address}")),
      "NO_PROXY" => Some("localhost".to_string()),
      "SSL_CERT_FILE" => tls.settings.cert_file.as_ref().map(|path| path.to_string_lossy().into_owned()),
      _ => None,
    });
    assert!(
      !crate::transport::proxy_settings(&settings)
        .expect("build proxy matcher")
        .expect("proxy is configured")
        .should_proxy_wss_host("localhost", target.port()),
      "NO_PROXY bypasses the target host",
    );
    let url = format!("wss://localhost:{}/socket", target.port());
    let options = options(Some(1024), Duration::from_secs(1), None, None);
    let (sender, mut receiver) = connect(&url, &options, &settings).await.expect("bypass the proxy");
    timeout(Duration::from_secs(2), sender.send(Message::Text("direct".to_string())))
      .await
      .expect("send completes")
      .expect("send text");
    assert_eq!(
      timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("echo arrives")
        .expect("receive echo")
        .expect("text succeeds"),
      Message::Text("direct".to_string())
    );
    timeout(Duration::from_secs(2), sender.close())
      .await
      .expect("close completes")
      .expect("close the direct connection");
    assert!(proxy_watch.await.expect("proxy watch finishes").is_err(), "NO_PROXY bypasses the proxy");
    timeout(Duration::from_secs(2), server).await.expect("TLS server finishes").expect("TLS server succeeds");
  }

  #[tokio::test]
  async fn oversize_message_closes_with_1009_and_fails_the_read() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the raw WebSocket peer");
    let address = listener.local_addr().expect("read peer address");
    let server = tokio::spawn(async move {
      let (stream, _) = listener.accept().await.expect("accept the client");
      let mut reader = raw_websocket(stream).await;
      write_raw_text(&mut reader, b"12345").await;
      close_code(&read_raw_frame(&mut reader).await)
    });
    let options = options(Some(4), Duration::from_secs(1), None, None);
    let url = format!("ws://{address}/socket");
    let (sender, mut receiver) = connect(&url, &options, &Settings::default()).await.expect("connect to peer");
    let error = receiver.recv().await.expect("receive the size failure").expect_err("oversize read fails");
    assert!(error.to_string().contains("1009 (message too big) frame with 5 bytes exceeds limit of 4 bytes"));
    assert_eq!(server.await.expect("raw peer finishes"), 1009);
    sender.abort();
  }

  #[tokio::test]
  async fn missing_pong_closes_with_1011_and_surfaces_the_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the raw WebSocket peer");
    let address = listener.local_addr().expect("read peer address");
    let server = tokio::spawn(async move {
      let (stream, _) = listener.accept().await.expect("accept the client");
      let mut reader = raw_websocket(stream).await;
      assert_eq!(read_raw_frame(&mut reader).await.opcode, 9, "client sends a ping");
      let close = read_raw_frame(&mut reader).await;
      (close_code(&close), String::from_utf8(close.payload[2..].to_vec()).expect("close reason is UTF-8"))
    });
    let options =
      options(Some(1024), Duration::from_secs(1), Some(Duration::from_millis(20)), Some(Duration::from_millis(60)));
    let url = format!("ws://{address}/socket");
    let (_sender, mut receiver) = connect(&url, &options, &Settings::default()).await.expect("connect to peer");
    let error = receiver.recv().await.expect("receive keepalive error").expect_err("missing pong fails the connection");
    assert_eq!(error.to_string(), "sent 1011 (internal error) keepalive ping timeout; no close frame received");
    assert_eq!(server.await.expect("raw peer finishes"), (1011, "keepalive ping timeout".to_string()));
  }

  #[tokio::test]
  async fn close_timeout_drops_a_connection_when_the_peer_does_not_reply() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the raw WebSocket peer");
    let address = listener.local_addr().expect("read peer address");
    let server = tokio::spawn(async move {
      let (stream, _) = listener.accept().await.expect("accept the client");
      let mut reader = raw_websocket(stream).await;
      let close = read_raw_frame(&mut reader).await;
      tokio::time::sleep(Duration::from_millis(250)).await;
      close_code(&close)
    });
    let options = options(Some(1024), Duration::from_millis(60), None, None);
    let url = format!("ws://{address}/socket");
    let (sender, _receiver) = connect(&url, &options, &Settings::default()).await.expect("connect to peer");
    let start = Instant::now();
    sender.close().await.expect("close returns when its deadline expires");
    assert!(start.elapsed() >= Duration::from_millis(50));
    assert!(start.elapsed() < Duration::from_millis(200));
    assert_eq!(server.await.expect("raw peer finishes"), 1000);
  }

  #[tokio::test]
  async fn opening_timeout_covers_a_server_that_never_answers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the stalled peer");
    let address = listener.local_addr().expect("read peer address");
    let server = tokio::spawn(async move {
      let (_stream, _) = listener.accept().await.expect("accept the client");
      tokio::time::sleep(Duration::from_millis(150)).await;
    });
    let mut options = options(Some(1024), Duration::from_secs(1), None, None);
    options.open_timeout = Some(Duration::from_millis(30));
    let url = format!("ws://{address}/socket");
    let error = match connect(&url, &options, &Settings::default()).await {
      Err(error) => error,
      Ok(_) => panic!("opening handshake should time out"),
    };
    assert_eq!(error.to_string(), "timed out during opening handshake");
    server.await.expect("stalled peer finishes");
  }
}
