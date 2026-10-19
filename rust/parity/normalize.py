"""Normalization rules: every value two correct servers cannot produce identically, and nothing else.

Each field rule names the field it masks and the reason the value cannot match. A rule replaces the value with a
placeholder and never removes the field, so a field one server omits still shows as a difference. Five kinds
of rule live here:

- the tree path rule rewrites each server's temporary tree root inside every string, header value and file
  path, because each server owns its own tree (the harness gives both roots one length, so sizes, offsets
  and the ETags and revisions derived from them still compare exactly);
- the minted UUID rule leaves fixture UUIDs intact and maps each other UUID by first appearance in sorted paths
  and document order, because requests and runs create different random ids in each tree;
- field rules mask one JSON field of one route's response body or of one home file;
- text rules mask one pattern in one route's non-JSON body (a page that embeds JSON in its HTML);
- header rules name the response headers the comparison leaves out.

Add a rule only for a value that differs for a reason no implementation can control (wall-clock time, a random
id, a pid, a duration, a per-server path). A field that differs because of behavior is a parity difference.
"""

import dataclasses
import fnmatch
import json
import re
from typing import Any

import yaml

TREE_PLACEHOLDER = "<tree>"
_UUID = re.compile(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b")
_VOICE_STEM = re.compile(r"\b\d{4}-\d{2}-\d{2}T\d{6}\.\d{3}Z_[0-9a-f]{8}\b")
_MERGED_TRACE_CACHE_PATH = re.compile(r"^home/cache/perfetto_merge/[0-9a-f]{64}\.json\.gz$")
_TEMP_DIR_SUFFIX = re.compile(r"\bcharliebot-(?:explain|oneshot|run)-(?P<value>[a-z0-9_]{8})(?![a-z0-9_])")


@dataclasses.dataclass(frozen=True)
class TextPatternRule:
  name: str
  pattern: re.Pattern[str]
  reason: str


VOICE_STEM_RULE = TextPatternRule(
    "voice-archive-stem",
    _VOICE_STEM,
    "wall-clock milliseconds and uuid4 hex: the voice archive names each recording at upload time",
)

# The prefix names the directory's role; tempfile.mkdtemp draws eight random characters from [a-z0-9_].
TEMP_DIR_SUFFIX_RULE = TextPatternRule(
    "temp-dir-suffix",
    _TEMP_DIR_SUFFIX,
    "tempfile.mkdtemp random suffix; the directory's role is fixed by its prefix",
)


@dataclasses.dataclass(frozen=True)
class PathPatternRule:
  name: str
  pattern: re.Pattern[str]
  reason: str

  @property
  def placeholder(self) -> str:
    return f"<{self.name}>"


MERGED_TRACE_CACHE_PATH_RULE = PathPatternRule(
    "merged-trace-cache-key",
    _MERGED_TRACE_CACHE_PATH,
    "the cache key hashes each tree's absolute trace paths and file times; the merged gzip bytes still compare",
)


@dataclasses.dataclass(frozen=True)
class FieldRule:
  """Mask *field* of the JSON documents *target* names.

  *target* is a route template ("GET /api/internal/version") for a response rule, or a glob over the
  tree-relative path ("home/state/writer_identity.json") of a JSON, JSONL or YAML file for a file rule.
  *field* is a dotted path; ``[*]`` steps into every list element and ``*`` into every mapping value. With
  *when*, the field is masked only in an object whose own fields hold those values.
  """

  name: str
  target: str
  field: str
  reason: str
  when: tuple[tuple[str, str], ...]

  @property
  def placeholder(self) -> str:
    return f"<{self.name}>"


@dataclasses.dataclass(frozen=True)
class TextRule:
  """Mask group ``value`` of every match of *pattern* in the non-JSON body of route *target*."""

  name: str
  target: str
  pattern: str
  reason: str

  @property
  def placeholder(self) -> str:
    return f"<{self.name}>"


_DELIVERY_STAMP = "the worker transcript stamps its closing run_delivery row with the time of the read"
_EXPLAIN_STAMP = (
    "wall-clock time: each server stamps an explanation's request and its finish on its own clock; "
    "a finish time is null until the generation ends")
# A finish time is null while its entry is pending, and that null stays compared. A `when` holds one value per key,
# so each ended state gets its own rule.
_ENDED_STATES = ("ready", "error")
_WEB_STATUS_STAMP = "wall-clock time of the write"

RESPONSE_RULES: tuple[FieldRule, ...] = (
    FieldRule(
        name="server-start-time",
        target="GET /api/internal/version",
        field="started_at",
        when=(),
        reason="wall-clock time: the moment each server process started",
    ),
    FieldRule(
        name="trigger-fire-time",
        target="POST /api/internal/schedule-trigger",
        field="fire_at",
        when=(),
        reason="wall-clock time: the trigger's due time is computed from each request's clock",
    ),
    *(
        FieldRule(
            name="delivery-read-time",
            target=route,
            field="messages[*].timestamp",
            when=(("role", "run_delivery"),),
            reason=f"wall-clock time: {_DELIVERY_STAMP}",
        ) for route in (
            "GET /api/sessions/{session_id}/bootstrap",
            "GET /api/sessions/{session_id}/events",
            "GET /api/sessions/{session_id}/transcript",
        )),
    *(
        FieldRule(
            name=f"explain-{field.replace('_', '-')}",
            target=route,
            field=field,
            when=(),
            reason=_EXPLAIN_STAMP,
        ) for route in (
            "POST /api/sessions/{session_id}/explain",
            "GET /api/sessions/{session_id}/explain",
        ) for field in ("requested_at",)),
    *(
        FieldRule(
            name="explain-generated-at",
            target=route,
            field="generated_at",
            when=(("state", state),),
            reason=_EXPLAIN_STAMP,
        ) for route in (
            "POST /api/sessions/{session_id}/explain",
            "GET /api/sessions/{session_id}/explain",
        ) for state in _ENDED_STATES),
    *(
        FieldRule(
            name="explain-status-generated-time",
            target="GET /api/sessions/{session_id}/explain/status",
            field="*.generated_at",
            when=(("state", state),),
            reason=_EXPLAIN_STAMP,
        ) for state in _ENDED_STATES),
    FieldRule(
        name="web-status-set-time",
        target="POST /api/internal/web/status",
        field="updated_at",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-read-time",
        target="GET /api/internal/web/status",
        field="web_status.updated_at",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-read-descendant-time",
        target="GET /api/internal/web/status",
        field="descendants[*].updated_at",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
)

WS_FRAME_RULES: dict[str, tuple[FieldRule, ...]] = {}

RESPONSE_TEXT_RULES: tuple[TextRule, ...] = (
    # The page embeds the session bootstrap as sorted-key JSON: the delivery row's run_id, task_state and
    # timestamp follow its role.
    TextRule(
        name="delivery-read-time",
        target="GET /",
        pattern=r'"role": "run_delivery", "run_id": "[^"]*", "task_state": "[^"]*", "timestamp": (?P<value>"[^"]*")',
        reason=f"wall-clock time: {_DELIVERY_STAMP}",
    ),)

FILE_RULES: tuple[FieldRule, ...] = (
    FieldRule(
        name="server-port",
        target="home/config.yaml",
        field="server.port",
        when=(),
        reason="each server listens on its own port, written into its own config by the harness",
    ),
    FieldRule(
        name="holder-pid",
        target="home/state/writer_identity.json",
        field="pid",
        when=(),
        reason="pid: the writer fence names the process that holds it",
    ),
    FieldRule(
        name="holder-pid-start",
        target="home/state/writer_identity.json",
        field="pid_start",
        when=(),
        reason="pid: the holder's /proc start time in clock ticks since boot",
    ),
    FieldRule(
        name="holder-start-time",
        target="home/state/writer_identity.json",
        field="started_at",
        when=(),
        reason="wall-clock time: the moment the holder acquired the fence",
    ),
    FieldRule(
        name="holder-argv",
        target="home/state/writer_identity.json",
        field="argv",
        when=(),
        reason="the holder's own command line: each implementation is a different executable",
    ),
    FieldRule(
        name="run-pid",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="pid",
        when=(),
        reason="pid: each server launches its own agent process",
    ),
    FieldRule(
        name="run-pid-start",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="pid_start",
        when=(),
        reason="pid: the agent's /proc start time in clock ticks differs between processes",
    ),
    FieldRule(
        name="run-start-time",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="started_at",
        when=(),
        reason="wall-clock time: each server records its agent launch on its own clock",
    ),
    FieldRule(
        name="run-end-time",
        target="home/sessions/*/data/runs/*/metadata.json",
        field="ended_at",
        when=(),
        reason="wall-clock time: each server records its agent completion on its own clock",
    ),
    FieldRule(
        name="chat-event-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="timestamp",
        when=(),
        reason="wall-clock time: each process stamps its own chat and run facts",
    ),
    FieldRule(
        name="master-done-total-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="total_ms",
        when=(("type", "master_done"),),
        reason="duration: server scheduling and trace timing differ between independent runs",
    ),
    FieldRule(
        name="master-done-first-event-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="spawn_to_first_event_ms",
        when=(("type", "master_done"),),
        reason="duration: process startup and event polling differ between independent runs",
    ),
    FieldRule(
        name="master-done-first-assistant-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="first_assistant_ms",
        when=(("type", "master_done"),),
        reason="duration: process startup and event polling differ between independent runs",
    ),
    FieldRule(
        name="master-done-thinking-seconds",
        target="home/sessions/*/data/chat_events.jsonl",
        field="thinking_seconds",
        when=(("type", "master_done"),),
        reason="duration: whole seconds of the busy interval, timed on each server's clock, differ between runs",
    ),
    FieldRule(
        name="session-updated-time",
        target="home/sessions/*/metadata.json",
        field="updated_at",
        when=(),
        reason="wall-clock time: the session records each server's latest event write",
    ),
    FieldRule(
        name="web-status-file-time",
        target="home/sessions/*/metadata.json",
        field="web_status.updated_at",
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="web-status-event-time",
        target="home/sessions/*/data/chat_events.jsonl",
        field="web_status.updated_at",
        # `when` sees the status object that holds this leaf; only web_status events carry that object.
        when=(),
        reason=_WEB_STATUS_STAMP,
    ),
    FieldRule(
        name="session-claude-start-time",
        target="home/sessions/*/metadata.json",
        field="cc_session_started_at",
        when=(),
        reason="wall-clock time: the session records when each server created its Claude conversation",
    ),
    *(
        FieldRule(
            name=f"trigger-{field.replace('_', '-')}",
            target="home/sessions/*/triggers/*.json",
            field=field,
            when=(),
            reason=f"wall-clock time: the trigger store records {field} on each server's clock",
        ) for field in ("created_at", "fire_at", "fired_at")),
    FieldRule(
        name="explain-result-requested-time",
        target="home/sessions/*/data/explain_results.json",
        field="*.requested_at",
        when=(),
        reason=_EXPLAIN_STAMP,
    ),
    *(
        FieldRule(
            name="explain-result-generated-time",
            target="home/sessions/*/data/explain_results.json",
            field="*.generated_at",
            when=(("state", state),),
            reason=_EXPLAIN_STAMP,
        ) for state in _ENDED_STATES),
    FieldRule(
        name="fake-agent-pid",
        target="fake-agent/spawns.jsonl",
        field="pid",
        when=(),
        reason="pid: each harness tree starts its fake agent as a different process",
    ),
    FieldRule(
        name="fake-agent-pid-start",
        target="fake-agent/spawns.jsonl",
        field="pid_start",
        when=(),
        reason="pid: each fake agent has a different /proc start time in clock ticks",
    ),
)

# Response headers outside the comparison, each with the reason it cannot match or carries nothing a client
# reads. Every other header is compared after the tree path rule.
IGNORED_HEADERS: dict[str, str] = {
    "date": "wall-clock time of the response",
    "server": "the HTTP stack's own banner; no client reads it",
    "content-length":
        "framing: the decoded body is compared, and a compressed body's length depends on the "
        "compressor",
    "transfer-encoding": "framing: chunked or sized delivery of the same body",
    "connection": "framing: connection reuse",
    "keep-alive": "framing: connection reuse",
}


@dataclasses.dataclass(frozen=True)
class ResponseHeaderRule:
  name: str
  target: str
  header: str
  reason: str

  @property
  def placeholder(self) -> str:
    return f"<{self.name}>"


RESPONSE_HEADER_RULES: tuple[ResponseHeaderRule, ...] = (
    ResponseHeaderRule(
        "merged-trace-etag",
        "GET /perfetto/merged",
        "etag",
        "the generated gzip file's mtime and size; its tree-specific path has a separate cache file",
    ),
    ResponseHeaderRule(
        "merged-trace-last-modified",
        "GET /perfetto/merged",
        "last-modified",
        "wall-clock time when each server created its identical merged gzip bytes",
    ),
)

# The headers each fake's feature code sets explicitly on its REST requests. No feature client sets a header on a
# WebSocket connect, so WebSocket handshakes compare none.
EXPLICIT_REQUEST_HEADERS: dict[str, tuple[str, ...]] = {
    "slack": ("authorization",),
    "discord": ("authorization", "user-agent"),
    "gemini": (),
    "muse": (),
    "openai_compatible": ("authorization",),
}

# Request headers compared by media type, lowercased and without parameters.
MEDIA_TYPE_REQUEST_HEADERS: dict[str, str] = {
    "content-type":
        "the media type names the body encoding the feature code chose; parameters such as the charset "
        "and the multipart boundary are library defaults and are dropped",
}

IGNORED_REQUEST_HEADERS: dict[str, str] = {
    "host": "transport routing: CONNECT already chooses the fake host; it carries no request behavior",
    "accept": "HTTP client default: the feature request does not set this header",
    "user-agent": "library default on Slack REST calls and on every WebSocket handshake; only Discord REST sets it",
    "accept-encoding": "HTTP client default: automatic response decoding is outside the feature request",
    "content-length": "framing: the parsed request body is compared structurally",
    "connection": "framing: HTTP connection reuse is independent of the feature request",
    "keep-alive": "framing: HTTP connection reuse is independent of the feature request",
    "expect": "transport negotiation: no feature request depends on a 100-continue exchange",
    "proxy-connection": "proxy transport: generated for the local CONNECT tunnel",
    "upgrade": "WebSocket framing: handshake type is already recorded separately",
    "sec-websocket-key": "WebSocket handshake nonce: required by the protocol and random per connection",
    "sec-websocket-version": "WebSocket protocol version: fixed negotiation metadata",
    "sec-websocket-extensions": "WebSocket extension negotiation: library-generated framing metadata",
}


@dataclasses.dataclass(frozen=True)
class IgnoredMessageRule:
  name: str
  fake: str
  direction: str
  field: str
  value: Any
  reason: str


IGNORED_MESSAGE_RULES: tuple[IgnoredMessageRule, ...] = (
    IgnoredMessageRule(
        "discord-heartbeat",
        "discord",
        "server_to_fake",
        "op",
        1,
        "Discord op 1 heartbeat timing includes an unseeded random first interval",
    ),)


def ignored_fake_message(record: dict) -> bool:
  """Whether a recorded WebSocket message matches a narrowly named ignored-frame rule."""
  message = record.get("message")
  if not isinstance(message, dict):
    return False
  return any(
      record.get("fake") == rule.fake and record.get("direction") == rule.direction and
      message.get(rule.field) == rule.value for rule in IGNORED_MESSAGE_RULES)


@dataclasses.dataclass
class Context:
  """One server's per-run values the tree path rule rewrites."""

  tree_root: str
  fixture_uuids: frozenset[str] = frozenset()
  minted_uuids: dict[str, str] = dataclasses.field(default_factory=dict)


def fixture_uuids(files: list[tuple[str, bytes]]) -> frozenset[str]:
  """UUID strings already present in the fixture tree's paths and file contents."""
  found: set[str] = set()
  for path, data in files:
    found.update(_UUID.findall(path))
    found.update(_UUID.findall(data.decode("utf-8", errors="surrogateescape")))
  return frozenset(found)


def _observe(value: Any, ctx: Context) -> None:
  if isinstance(value, str):
    for match in _UUID.finditer(value):
      identifier = match[0]
      if identifier not in ctx.fixture_uuids:
        ctx.minted_uuids.setdefault(identifier, f"<minted-{len(ctx.minted_uuids) + 1}>")
  elif isinstance(value, list):
    for item in value:
      _observe(item, ctx)
  elif isinstance(value, dict):
    for key, item in value.items():
      _observe(key, ctx)
      _observe(item, ctx)


def prepare_snapshot(entries: list[tuple[str, str, bytes]], ctx: Context) -> None:
  """Assign minted UUID names by sorted tree path and each JSON document's stored order."""
  for path, kind, data in sorted(entries, key=lambda entry: entry[0]):
    _observe(path, ctx)
    if kind != "file":
      continue
    if path.endswith(".json"):
      _observe(json.loads(data), ctx)
    elif path.endswith(".jsonl"):
      for line in data.splitlines():
        if line.strip():
          try:
            value = json.loads(line)
          except json.JSONDecodeError:
            continue
          _observe(value, ctx)
    elif path.endswith((".yaml", ".yml")):
      _observe(yaml.safe_load(data), ctx)
    else:
      _observe(data.decode("utf-8", errors="surrogateescape"), ctx)


def _mask_temp_dir_suffix(match: re.Match[str]) -> str:
  """The match up to its suffix, then the placeholder: the prefix still names the directory's role."""
  return match[0][:match.start("value") - match.start()] + f"<{TEMP_DIR_SUFFIX_RULE.name}>"


def normalize_text(text: str, ctx: Context) -> str:
  """The tree path, minted UUID and scratch directory rules: each server-specific value reads as a placeholder."""
  normalized = text.replace(ctx.tree_root, TREE_PLACEHOLDER)

  def replace_uuid(match: re.Match[str]) -> str:
    identifier = match[0]
    if identifier in ctx.fixture_uuids:
      return identifier
    return ctx.minted_uuids.setdefault(identifier, f"<minted-{len(ctx.minted_uuids) + 1}>")

  normalized = VOICE_STEM_RULE.pattern.sub(f"<{VOICE_STEM_RULE.name}>", normalized)
  normalized = TEMP_DIR_SUFFIX_RULE.pattern.sub(_mask_temp_dir_suffix, normalized)
  return _UUID.sub(replace_uuid, normalized)


def normalize_path(path: str, ctx: Context) -> str:
  """Normalize one tree-relative or absolute file path, including the voice archive name rule."""
  normalized = normalize_text(path, ctx)
  if MERGED_TRACE_CACHE_PATH_RULE.pattern.fullmatch(normalized):
    return MERGED_TRACE_CACHE_PATH_RULE.placeholder
  return normalized


def _normalize_strings(value: Any, ctx: Context) -> Any:
  if isinstance(value, str):
    return normalize_text(value, ctx)
  if isinstance(value, list):
    return [_normalize_strings(item, ctx) for item in value]
  if isinstance(value, dict):
    return {normalize_text(key, ctx): _normalize_strings(item, ctx) for key, item in value.items()}
  return value


def _mask(value: Any, segments: list[str], rule: FieldRule) -> Any:
  if not segments:
    return rule.placeholder
  head, rest = segments[0], segments[1:]
  if head == "[*]":
    return [_mask(item, rest, rule) for item in value] if isinstance(value, list) else value
  if not isinstance(value, dict):
    return value
  if head == "*":
    return {key: _mask(item, rest, rule) for key, item in value.items()}
  if head not in value:
    return value
  if not rest and any(value.get(key) != wanted for key, wanted in rule.when):
    return value
  return {**value, head: _mask(value[head], rest, rule)}


def _split_field(field: str) -> list[str]:
  segments: list[str] = []
  for part in field.split("."):
    if part.endswith("[*]"):
      if part[:-3]:
        segments.append(part[:-3])
      segments.append("[*]")
    else:
      segments.append(part)
  return segments


def normalize_json(value: Any, rules: tuple[FieldRule, ...], ctx: Context) -> Any:
  """Apply the tree path rule to every string, then every field rule in *rules*."""
  value = _normalize_strings(value, ctx)
  for rule in rules:
    value = _mask(value, _split_field(rule.field), rule)
  return value


def normalize_body_bytes(body: bytes, route: str, ctx: Context) -> bytes:
  """A non-JSON body: the tree path rule, then every text rule of *route*."""
  body = normalize_text(body.decode("utf-8", errors="surrogateescape"), ctx).encode("utf-8", errors="surrogateescape")
  for rule in RESPONSE_TEXT_RULES:
    if rule.target == route:
      placeholder = f'"{rule.placeholder}"'.encode()

      def mask(match: re.Match[bytes], placeholder: bytes = placeholder) -> bytes:
        start, end = match.span("value")
        return match.string[match.start():start] + placeholder + match.string[end:match.end()]

      body = re.sub(rule.pattern.encode(), mask, body)
  return body


# The message id the proxy mints for each answer: `msg_` and 32 lowercase hex digits, from uuid4. It is the `id` of a
# proxy response's JSON answer and of a stream's message_start frame, and each server mints its own, so the rule masks
# it there and nowhere else. An id the upstream supplies has another form and stays visible: a proxy that copied the
# upstream's id would still show as a difference.
PROXY_MESSAGE_ID = re.compile(rb'"id":"msg_[0-9a-f]{32}"')
PROXY_MESSAGE_ID_PLACEHOLDER = b'"id":"<proxy-message-id>"'
PROXY_ROUTE = re.compile(r"POST /api/anthropic-proxy/openai-compatible/[^/]+/v1/messages")


def mask_proxy_message_ids(body: bytes, route: str) -> bytes:
  """Mask each message id a proxy route minted in its body; the other routes' bodies are returned unchanged."""
  if PROXY_ROUTE.fullmatch(route) is None:
    return body
  return PROXY_MESSAGE_ID.sub(PROXY_MESSAGE_ID_PLACEHOLDER, body)


def response_rules(route: str) -> tuple[FieldRule, ...]:
  return tuple(rule for rule in RESPONSE_RULES if rule.target == route)


def ws_frame_rules(route: str) -> tuple[FieldRule, ...]:
  """Frame rules are keyed by the WebSocket route template, matching response-rule selection."""
  return WS_FRAME_RULES.get(route, ())


def file_rules(relative_path: str) -> tuple[FieldRule, ...]:
  return tuple(rule for rule in FILE_RULES if fnmatch.fnmatchcase(relative_path, rule.target))


def normalize_headers(headers: list[tuple[str, str]], ctx: Context, route: str) -> dict[str, list[str]]:
  """The compared headers: lowercased names, ignored ones dropped, values under the tree path rule."""
  compared: dict[str, list[str]] = {}
  for name, value in headers:
    lowered = name.lower()
    if lowered in IGNORED_HEADERS:
      continue
    rule = next(
        (rule for rule in RESPONSE_HEADER_RULES if rule.target == route and rule.header == lowered),
        None,
    )
    compared.setdefault(lowered, []).append(rule.placeholder if rule else normalize_text(value, ctx))
  return compared


def _media_type(value: str) -> str:
  """A Content-Type value without its parameters, lowercased."""
  return value.split(";", 1)[0].strip().lower()


def request_headers(
    fake: str,
    headers: dict[str, list[str]],
    ctx: Context,
    websocket: bool,
) -> dict[str, list[str]]:
  """The headers the feature code sets explicitly, and each media-type header's media type.

  Every other header must be named in IGNORED_REQUEST_HEADERS.
  """
  explicit = () if websocket else EXPLICIT_REQUEST_HEADERS[fake]
  compared: dict[str, list[str]] = {}
  for name, values in headers.items():
    if name in explicit:
      compared[name] = [normalize_text(value, ctx) for value in values]
    elif name in MEDIA_TYPE_REQUEST_HEADERS:
      compared[name] = [_media_type(value) for value in values]
    elif name not in IGNORED_REQUEST_HEADERS:
      raise ValueError(f"{fake} request header {name!r} is neither set explicitly nor named in IGNORED_REQUEST_HEADERS")
  return compared
