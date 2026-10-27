"""A stand-in for the opencode 2.0 binary, for the backend tests.

``serve --port 0`` speaks the shared server's HTTP surface on loopback: the routes the backend
calls, one global SSE stream, and a control route the test uses to inject events, drop the
stream, or end the process. ``run`` prints the NDJSON lines of a one-shot run.

Behaviour comes from the JSON file named by FAKE_OC_SCRIPT: ``prompts`` maps a prompt's text to a
step with ``events`` (session events, sessionID filled in), ``shells`` (running shell commands the
session starts) and ``hold`` (keep the execution running until an interrupt). ``release_status``
makes DELETE /api/debug/location answer that status instead of 204. Every request is appended to
the FAKE_OC_LOG file as one JSON line, tagged with the serving process's pid.

The loaded locations are the directories a session create, a shell list or a model list named;
GET /api/debug/location lists them and DELETE /api/debug/location unloads one.
"""

import asyncio
import json
import os
import pathlib
import sys
import urllib.parse

SCRIPT = json.load(open(os.environ["FAKE_OC_SCRIPT"])) if os.environ.get("FAKE_OC_SCRIPT") else {}
LOG = os.environ.get("FAKE_OC_LOG")
VERSION = os.environ.get("FAKE_OC_VERSION", "2.0.32")


def record(entry: dict) -> None:
  if LOG:
    with open(LOG, "a") as f:
      f.write(json.dumps({"pid": os.getpid(), **entry}) + "\n")


class FakeServer:

  def __init__(self) -> None:
    self.sessions: dict[str, dict] = {}
    self.shells: dict[str, dict] = {}
    self.active: set[str] = set()
    self.loaded: set[str] = set()
    self.subscribers: list[asyncio.Queue] = []
    self.next_id = 0

  def emit(self, event: dict) -> None:
    for queue in self.subscribers:
      queue.put_nowait(event)

  def new_id(self, prefix: str) -> str:
    self.next_id += 1
    return f"{prefix}_{self.next_id}"

  async def run_prompt(self, session_id: str, body: dict) -> None:
    step = SCRIPT.get("prompts", {}).get(body.get("text"), {})
    record({"prompt_text": body.get("text"), "prompt_files": len(body.get("files", [])), "session": session_id})
    self.active.add(session_id)
    self.emit({"type": "session.execution.started", "data": {"sessionID": session_id}})
    for _ in step.get("shells", []):
      shell_id = self.new_id("sh")
      self.shells[shell_id] = {
          "id": shell_id,
          "status": "running",
          "metadata": {
              "sessionID": session_id
          },
          "directory": self.sessions[session_id]["directory"]
      }
    for event in step.get("events", []):
      event = json.loads(json.dumps(event))
      event.setdefault("data", {})["sessionID"] = session_id
      self.emit(event)
    if step.get("hold"):
      return
    if session_id in self.active:
      self.active.discard(session_id)
      self.emit({"type": "session.execution.succeeded", "data": {"sessionID": session_id}})

  async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    head = await reader.readuntil(b"\r\n\r\n")
    lines = head.decode().split("\r\n")
    method, target, _ = lines[0].split(" ", 2)
    headers = {k.strip().lower(): v.strip() for k, v in (line.split(":", 1) for line in lines[1:] if ":" in line)}
    length = int(headers.get("content-length", 0))
    body = json.loads(await reader.readexactly(length)) if length else None
    path, _, query = target.partition("?")
    params = urllib.parse.parse_qs(query)
    record({"method": method, "path": path, "query": params, "body": body, "auth": headers.get("authorization", "")})

    if method == "GET" and path == "/api/event":
      await self.stream(writer)
      return
    if path == "/__control":
      await self.control(writer, body)
      return
    status, payload = self.route(method, path, params, body)
    data = json.dumps(payload).encode() if payload is not None else b""
    writer.write(
        f"HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {len(data)}\r\n"
        f"Connection: close\r\n\r\n".encode() + data)
    await writer.drain()
    writer.close()

  def route(self, method: str, path: str, params: dict, body: dict | None) -> tuple[int, object]:
    parts = path.strip("/").split("/")
    if method == "GET" and path == "/api/info":
      return 200, {"version": VERSION}
    if method == "GET" and path == "/api/debug/location":
      return 200, [{"directory": directory} for directory in sorted(self.loaded)]
    if method == "DELETE" and path == "/api/debug/location":
      directory = params.get("location[directory]", [""])[0]
      status = SCRIPT.get("release_status", 204)
      if 200 <= status < 300:
        self.loaded.discard(directory)
      return status, None
    if method == "GET" and path == "/api/model":
      self.loaded.add(params.get("location[directory]", [""])[0])
      return 200, {
          "data": [{
              "id": "fake-model",
              "providerID": "fakeprov",
              "limit": {
                  "context": 200000,
                  "output": 8000
              }
          }]
      }
    if method == "POST" and path == "/api/session":
      session_id = self.new_id("ses")
      self.sessions[session_id] = {"directory": body["location"]["directory"], "environment": None}
      self.loaded.add(body["location"]["directory"])
      return 200, {"data": {"id": session_id}}
    if len(parts) >= 3 and parts[:2] == ["api", "session"]:
      session_id = parts[2]
      if session_id not in self.sessions:
        return 404, {"_tag": "SessionNotFoundError", "sessionID": session_id}
      action = parts[3] if len(parts) > 3 else None
      if method == "GET" and action is None:
        return 200, {"data": {"id": session_id}}
      if method == "PUT" and action == "environment":
        self.sessions[session_id]["environment"] = body["variables"]
        return 204, None
      if method == "POST" and action == "prompt":
        asyncio.get_running_loop().create_task(self.run_prompt(session_id, body))
        return 200, {"data": {"id": self.new_id("msg")}}
      if method == "POST" and action == "interrupt":
        if session_id in self.active:
          self.active.discard(session_id)
          asyncio.get_running_loop().call_later(
              SCRIPT.get("interrupt_event_delay", 0.0), self.emit, {
                  "type": "session.execution.interrupted",
                  "data": {
                      "sessionID": session_id,
                      "reason": "user"
                  }
              })
          return 200, {"interrupted": True}
        return 200, {"interrupted": False}
    if method == "POST" and path == "/api/location/reload":
      return 204, None
    if method == "GET" and path == "/api/shell":
      directory = params.get("location[directory]", [""])[0]
      self.loaded.add(directory)
      return 200, {
          "location": {
              "directory": directory
          },
          "data": [shell for shell in self.shells.values() if shell["directory"] == directory]
      }
    if method == "DELETE" and len(parts) == 3 and parts[:2] == ["api", "shell"]:
      self.shells.pop(parts[2], None)
      return 204, None
    return 404, {"error": f"no route {method} {path}"}

  async def stream(self, writer: asyncio.StreamWriter) -> None:
    writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
    await asyncio.sleep(SCRIPT.get("event_subscribe_delay", 0))
    queue: asyncio.Queue = asyncio.Queue()
    self.subscribers.append(queue)
    queue.put_nowait({"type": "server.connected", "data": {}})
    try:
      while True:
        event = await queue.get()
        if event is None:
          break
        writer.write(f"data: {json.dumps(event)}\n\n".encode())
        await writer.drain()
    finally:
      self.subscribers.remove(queue)
      writer.close()

  async def control(self, writer: asyncio.StreamWriter, body: dict) -> None:
    writer.write(b"HTTP/1.1 204 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    await writer.drain()
    writer.close()
    if "activate" in body:
      self.active.add(body["activate"])
    if "emit" in body:
      self.emit(body["emit"])
    if body.get("drop_stream"):
      for queue in list(self.subscribers):
        queue.put_nowait(None)
    if "exit" in body:
      os._exit(body["exit"])


async def serve() -> None:
  fake = FakeServer()
  server = await asyncio.start_server(fake.handle, "127.0.0.1", 0)
  port = server.sockets[0].getsockname()[1]
  print(f"server listening on http://127.0.0.1:{port}", flush=True)
  async with server:
    await server.serve_forever()


def run_once() -> None:
  data_home = pathlib.Path(os.environ["XDG_DATA_HOME"])
  (data_home / "opencode" / "shell").mkdir(parents=True, exist_ok=True)
  record(
      {
          "run_args": sys.argv[2:],
          "xdg_data_home": str(data_home),
          "xdg_state_home": os.environ.get("XDG_STATE_HOME"),
          "xdg_cache_home": os.environ.get("XDG_CACHE_HOME"),
          "config": os.environ.get("OPENCODE_CONFIG_CONTENT")
      })
  for line in SCRIPT.get("run_lines", []):
    print(json.dumps(line), flush=True)
  sys.exit(SCRIPT.get("run_exit", 0))


if __name__ == "__main__":
  if sys.argv[1] == "serve":
    asyncio.run(serve())
  elif sys.argv[1] == "run":
    run_once()
  else:
    sys.exit(f"fake_opencode_serve: unknown command {sys.argv[1]!r}")
