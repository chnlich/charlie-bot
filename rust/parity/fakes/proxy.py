"""Terminate local CONNECT tunnels for fake provider hosts without forwarding any connection."""

import asyncio
import base64
import dataclasses
import email
import email.parser
import email.policy
import hashlib
import http
import json
import logging
import pathlib
import ssl
import struct
import urllib.parse
from collections.abc import Awaitable, Callable
from typing import Any

from . import recorder

LOGGER = logging.getLogger(__name__)
_HEADER_END = b"\r\n\r\n"
_WEBSOCKET_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
_MAX_HTTP_HEADER_BYTES = 65536
_TLS_HANDSHAKE_TIMEOUT_SECONDS = 10.0
_WEBSOCKET_CLOSE = 0x8
_WEBSOCKET_PING = 0x9
_WEBSOCKET_PONG = 0xA

HOST_FAKE = {
    "slack.com": "slack",
    "wss-primary.slack.com": "slack",
    "discord.com": "discord",
    "gateway.discord.gg": "discord",
    "generativelanguage.googleapis.com": "gemini",
    "api.meta.ai": "muse",
    "openai-compatible.parity.test": "openai_compatible",
}


@dataclasses.dataclass(frozen=True)
class Request:
  host: str
  method: str
  path: str
  query: dict[str, list[str]]
  headers: dict[str, list[str]]
  body: Any


@dataclasses.dataclass(frozen=True)
class HTTPResponse:
  status: int
  headers: dict[str, str]
  body: bytes


class WebSocketConnection:
  """One server-initiated fake WebSocket after the proxy terminates its TLS tunnel."""

  def __init__(
      self,
      reader: asyncio.StreamReader,
      writer: asyncio.StreamWriter,
      fake: str,
      connection_id: str,
      path: str,
      query: dict[str, list[str]],
      headers: dict[str, list[str]],
      traffic: recorder.Recorder,
  ) -> None:
    self.reader = reader
    self.writer = writer
    self.fake = fake
    self.connection_id = connection_id
    self.path = path
    self.query = query
    self.headers = headers
    self._traffic = traffic
    self._write_lock = asyncio.Lock()
    self._fragments: list[bytes] = []
    self._fragment_opcode: int | None = None

  async def recv(self) -> str | bytes | None:
    while True:
      first, second = await self.reader.readexactly(2)
      final = bool(first & 0x80)
      opcode = first & 0x0F
      masked = bool(second & 0x80)
      length = second & 0x7F
      if length == 126:
        length = struct.unpack("!H", await self.reader.readexactly(2))[0]
      elif length == 127:
        length = struct.unpack("!Q", await self.reader.readexactly(8))[0]
      mask = await self.reader.readexactly(4) if masked else b""
      payload = await self.reader.readexactly(length)
      if masked:
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
      if opcode == _WEBSOCKET_PING:
        await self._send_frame(_WEBSOCKET_PONG, payload)
        continue
      if opcode == _WEBSOCKET_PONG:
        continue
      if opcode == _WEBSOCKET_CLOSE:
        await self._send_frame(_WEBSOCKET_CLOSE, payload)
        return None
      if opcode in (1, 2):
        if self._fragment_opcode is not None:
          raise ValueError("a new WebSocket data frame arrived before its fragmented message ended")
        self._fragment_opcode = opcode
        self._fragments = [payload]
      elif opcode == 0 and self._fragment_opcode is not None:
        self._fragments.append(payload)
      else:
        raise ValueError(f"unsupported WebSocket opcode {opcode}")
      if not final:
        continue
      complete_opcode = self._fragment_opcode
      complete_payload = b"".join(self._fragments)
      self._fragment_opcode = None
      self._fragments = []
      if complete_opcode == 1:
        text = complete_payload.decode("utf-8")
        try:
          message: Any = json.loads(text)
        except json.JSONDecodeError:
          message = text
        self._traffic.record(
            {
                "kind": "ws_message",
                "fake": self.fake,
                "connection": self.connection_id,
                "path": self.path,
                "direction": "server_to_fake",
                "message_type": "text",
                "message": message,
            })
        return text
      self._traffic.record(
          {
              "kind": "ws_message",
              "fake": self.fake,
              "connection": self.connection_id,
              "path": self.path,
              "direction": "server_to_fake",
              "message_type": "binary",
              "message": complete_payload,
          })
      return complete_payload

  async def send_text(self, text: str) -> None:
    await self._send_frame(1, text.encode("utf-8"))

  async def send_binary(self, data: bytes) -> None:
    await self._send_frame(2, data)

  async def close(self) -> None:
    await self._send_frame(_WEBSOCKET_CLOSE, struct.pack("!H", 1001))

  async def _send_frame(self, opcode: int, payload: bytes) -> None:
    length = len(payload)
    if length < 126:
      header = bytes((0x80 | opcode, length))
    elif length < 65536:
      header = bytes((0x80 | opcode, 126)) + struct.pack("!H", length)
    else:
      header = bytes((0x80 | opcode, 127)) + struct.pack("!Q", length)
    async with self._write_lock:
      self.writer.write(header + payload)
      await self.writer.drain()


HTTPHandler = Callable[[str, Request], HTTPResponse]
WebSocketHandler = Callable[[str, WebSocketConnection], Awaitable[None]]
RefusalHandler = Callable[[str], None]
ConnectionId = Callable[[str], str]


class LocalHTTPSProxy:
  """Accept local CONNECT, deny unlisted hosts, and serve listed hosts from in-process fakes."""

  def __init__(
      self,
      certificate: pathlib.Path,
      private_key: pathlib.Path,
      allowed_fakes: frozenset[str],
      traffic: recorder.Recorder,
      http_handler: HTTPHandler,
      websocket_handler: WebSocketHandler,
      refusal_handler: RefusalHandler,
      connection_id: ConnectionId,
  ) -> None:
    self._traffic = traffic
    self._http_handler = http_handler
    self._websocket_handler = websocket_handler
    self._refusal_handler = refusal_handler
    self._connection_id = connection_id
    self._host_fakes = {host: fake for host, fake in HOST_FAKE.items() if fake in allowed_fakes}
    self._tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    self._tls.set_alpn_protocols(["http/1.1"])
    self._tls.load_cert_chain(certificate, private_key)
    self._server: asyncio.AbstractServer | None = None
    self._connections: set[WebSocketConnection] = set()
    self.port: int | None = None

  async def start(self) -> int:
    self._server = await asyncio.start_server(self._serve, "127.0.0.1", 0)
    self.port = int(self._server.sockets[0].getsockname()[1])
    return self.port

  async def close(self) -> None:
    if self._server is not None:
      self._server.close()
      await self._server.wait_closed()
    for connection in tuple(self._connections):
      await connection.close()
      connection.writer.close()

  async def _serve(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    try:
      request_line, _headers = await self._read_head(reader)
      method, target, _protocol = request_line.split(" ", 2)
      if method != "CONNECT":
        self._refuse(writer, target)
        self._refusal_handler(target)
        return
      host, separator, port = target.rpartition(":")
      fake = self._host_fakes.get(host.lower())
      if not separator or port != "443" or fake is None:
        self._refuse(writer, target)
        self._refusal_handler(target)
        return
      writer.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
      await writer.drain()
      await writer.start_tls(self._tls, ssl_handshake_timeout=_TLS_HANDSHAKE_TIMEOUT_SECONDS)
      await self._serve_tunnel(fake, reader, writer)
    except (asyncio.IncompleteReadError, ConnectionResetError, BrokenPipeError) as error:
      LOGGER.debug("fake proxy connection ended", exc_info=error)
    except Exception:
      LOGGER.exception("fake proxy connection failed")
      raise
    finally:
      writer.close()
      await writer.wait_closed()

  def _refuse(self, writer: asyncio.StreamWriter, target: str) -> None:
    del target
    writer.write(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")

  async def _serve_tunnel(self, fake: str, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    while True:
      try:
        request_line, headers = await self._read_head(reader)
      except asyncio.IncompleteReadError:
        return
      method, target, _protocol = request_line.split(" ", 2)
      parsed = urllib.parse.urlsplit(target)
      path = parsed.path or "/"
      query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)
      if headers.get("upgrade", [""])[-1].lower() == "websocket":
        connection = self._upgrade_websocket(fake, path, query, headers, reader, writer)
        self._connections.add(connection)
        try:
          await self._websocket_handler(fake, connection)
        finally:
          self._connections.discard(connection)
        return
      body = await self._read_body(reader, headers)
      request = Request(
          host=fake,
          method=method,
          path=path,
          query=query,
          headers=headers,
          body=_parse_body(body, headers),
      )
      self._traffic.record(
          {
              "kind": "http",
              "fake": fake,
              "method": method,
              "path": path,
              "query": query,
              "headers": headers,
              "body": request.body,
          })
      response = self._http_handler(fake, request)
      await self._send_http_response(writer, response)
      if headers.get("connection", [""])[-1].lower() == "close":
        return

  def _upgrade_websocket(
      self,
      fake: str,
      path: str,
      query: dict[str, list[str]],
      headers: dict[str, list[str]],
      reader: asyncio.StreamReader,
      writer: asyncio.StreamWriter,
  ) -> WebSocketConnection:
    key = headers["sec-websocket-key"][-1]
    accept = base64.b64encode(hashlib.sha1((key + _WEBSOCKET_GUID).encode("ascii")).digest()).decode("ascii")
    writer.write(
        (
            "HTTP/1.1 101 Switching Protocols\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Accept: {accept}\r\n\r\n").encode("ascii"))
    connection_id = self._connection_id(fake)
    self._traffic.record(
        {
            "kind": "ws_handshake",
            "fake": fake,
            "connection": connection_id,
            "path": path,
            "query": query,
            "headers": headers,
        })
    return WebSocketConnection(reader, writer, fake, connection_id, path, query, headers, self._traffic)

  async def _read_head(self, reader: asyncio.StreamReader) -> tuple[str, dict[str, list[str]]]:
    raw = await reader.readuntil(_HEADER_END)
    if len(raw) > _MAX_HTTP_HEADER_BYTES:
      raise ValueError(f"fake proxy HTTP header exceeds {_MAX_HTTP_HEADER_BYTES} bytes")
    lines = raw[:-4].decode("iso-8859-1").split("\r\n")
    headers: dict[str, list[str]] = {}
    for line in lines[1:]:
      name, separator, value = line.partition(":")
      if not separator:
        raise ValueError(f"malformed HTTP header {line!r}")
      headers.setdefault(name.strip().lower(), []).append(value.strip())
    return lines[0], headers

  async def _read_body(self, reader: asyncio.StreamReader, headers: dict[str, list[str]]) -> bytes:
    lengths = headers.get("content-length", ["0"])
    if len(lengths) != 1:
      raise ValueError(f"request has multiple Content-Length headers: {lengths}")
    length = int(lengths[0])
    return await reader.readexactly(length) if length else b""

  async def _send_http_response(self, writer: asyncio.StreamWriter, response: HTTPResponse) -> None:
    reason = http.HTTPStatus(response.status).phrase
    headers = {"Content-Length": str(len(response.body)), "Connection": "keep-alive", **response.headers}
    header_text = "".join(f"{name}: {value}\r\n" for name, value in headers.items())
    writer.write(f"HTTP/1.1 {response.status} {reason}\r\n{header_text}\r\n".encode("iso-8859-1") + response.body)
    await writer.drain()


def _parse_body(body: bytes, headers: dict[str, list[str]]) -> Any:
  if not body:
    return None
  content_type = headers.get("content-type", [""])[-1]
  media_type = content_type.split(";", 1)[0].strip().lower()
  if media_type == "application/json" or media_type.endswith("+json"):
    return json.loads(body)
  if media_type == "application/x-www-form-urlencoded":
    return urllib.parse.parse_qs(body.decode("utf-8"), keep_blank_values=True)
  if media_type == "multipart/form-data":
    envelope = (f"Content-Type: {content_type}\r\nMIME-Version: 1.0\r\n\r\n".encode("ascii") + body)
    message = email.parser.BytesParser(policy=email.policy.default).parsebytes(envelope)
    fields = []
    for part in message.iter_parts():
      name = part.get_param("name", header="content-disposition")
      filename = part.get_filename()
      payload = part.get_payload(decode=True)
      if filename is None:
        fields.append({"name": name, "value": payload.decode(part.get_content_charset() or "utf-8")})
      else:
        fields.append(
            {
                "name": name,
                "filename": filename,
                "content_type": part.get_content_type(),
                "content": payload,
            })
    return fields
  return body
