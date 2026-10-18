"""Normalization rules: every value two correct servers cannot produce identically, and nothing else.

Each rule names the field it masks and the reason the value cannot match. A rule replaces the value with a
placeholder and never removes the field, so a field one server omits still shows as a difference. Three kinds
of rule live here:

- the tree path rule rewrites each server's temporary tree root inside every string, header value and file
  path, because each server owns its own tree (the harness gives both roots one length, so sizes, offsets
  and the ETags and revisions derived from them still compare exactly);
- field rules mask one JSON field of one route's response body or of one home file;
- text rules mask one pattern in one route's non-JSON body (a page that embeds JSON in its HTML);
- header rules name the response headers the comparison leaves out.

Add a rule only for a value that differs for a reason no implementation can control (wall-clock time, a random
id, a pid, a duration, a per-server path). A field that differs because of behavior is a parity difference.
"""

import dataclasses
import fnmatch
import re
from typing import Any

TREE_PLACEHOLDER = "<tree>"


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

RESPONSE_RULES: tuple[FieldRule, ...] = (
    FieldRule(
        name="server-start-time",
        target="GET /api/internal/version",
        field="started_at",
        when=(),
        reason="wall-clock time: the moment each server process started",
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
)

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
class Context:
  """One server's per-run values the tree path rule rewrites."""

  tree_root: str


def normalize_text(text: str, ctx: Context) -> str:
  """The tree path rule: each server's tree root reads as the same placeholder."""
  return text.replace(ctx.tree_root, TREE_PLACEHOLDER)


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
  body = body.replace(ctx.tree_root.encode(), TREE_PLACEHOLDER.encode())
  for rule in RESPONSE_TEXT_RULES:
    if rule.target == route:
      placeholder = f'"{rule.placeholder}"'.encode()

      def mask(match: re.Match[bytes], placeholder: bytes = placeholder) -> bytes:
        start, end = match.span("value")
        return match.string[match.start():start] + placeholder + match.string[end:match.end()]

      body = re.sub(rule.pattern.encode(), mask, body)
  return body


def response_rules(route: str) -> tuple[FieldRule, ...]:
  return tuple(rule for rule in RESPONSE_RULES if rule.target == route)


def file_rules(relative_path: str) -> tuple[FieldRule, ...]:
  return tuple(rule for rule in FILE_RULES if fnmatch.fnmatchcase(relative_path, rule.target))


def normalize_headers(headers: list[tuple[str, str]], ctx: Context) -> dict[str, list[str]]:
  """The compared headers: lowercased names, ignored ones dropped, values under the tree path rule."""
  compared: dict[str, list[str]] = {}
  for name, value in headers:
    lowered = name.lower()
    if lowered in IGNORED_HEADERS:
      continue
    compared.setdefault(lowered, []).append(normalize_text(value, ctx))
  return compared
