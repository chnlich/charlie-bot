"""_LeanLineRenderer tests: byte-identical output against the dev ConsoleRenderer.

Every renderer test drives both renderers over the same event dict and asserts
the strings are equal, so a structlog upgrade that moves a padding or quoting
rule fails here. The composed access line (``log_http_request_line``) and the
per-minute stamp memo have their own sections below: the access line pins its
bytes against the lean renderer per value shape, the memo tests drive no
renderer at all.
"""

import contextlib
import io
import sys
import time

import pytest
import structlog
from structlog.dev import ConsoleRenderer

from src.core import log_once
from src.core.log_once import _LeanLineRenderer, _local_timestamp, log_http_request_line


def _render_both(event_dict: dict) -> tuple[str, str]:
  dev = ConsoleRenderer(colors=False)
  lean = _LeanLineRenderer()
  return lean(None, "info", dict(event_dict)), dev(None, "info", dict(event_dict))


@pytest.mark.parametrize(
    ("level", "event"), [
        ("debug", "http_request"),
        ("warning", "worker_stderr"),
        ("critical", "critical_event"),
        ("exception", "exception_level_name"),
    ])
def test_level_and_event_padding_matches(level: str, event: str) -> None:
  lean, dev = _render_both({"timestamp": "2026-09-18 19:04:40", "level": level, "event": event})
  assert lean == dev


def test_sorted_scalar_fields_match() -> None:
  event_dict = {
      "timestamp": "2026-09-18 19:04:40",
      "level": "info",
      "event": "http_request",
      "status": 401,
      "duration_ms": 3,
      "method": "GET",
      "path": "/api/sessions/status",
      "client": "127.0.0.1",
  }
  lean, dev = _render_both(event_dict)
  assert lean == dev
  assert "client=127.0.0.1 duration_ms=3 method=GET" in lean


@pytest.mark.parametrize(
    "value", [
        "plain",
        "with spaces",
        "with=equals",
        "tab\tinside",
        "quote'inside",
        'double"inside',
        "",
        "unicode-ümläut-✓",
        3.5,
        True,
        None,
        {
            "nested": 1,
            "b": 2
        },
        ["list", 2],
    ])
def test_value_rendering_matches(value: object) -> None:
  event_dict = {"timestamp": "2026-09-18 19:04:40", "level": "info", "event": "evt", "field": value}
  lean, dev = _render_both(event_dict)
  assert lean == dev


@pytest.mark.parametrize("event", ["e", "x" * 64], ids=["short", "long"])
def test_event_padding_matches(event: str) -> None:
  lean, dev = _render_both({"timestamp": "t", "level": "info", "event": event, "k": 1})
  assert lean == dev
  assert lean == "t [info     ] " + event.ljust(30) + " k=1"


def test_multiline_string_value_matches() -> None:
  lean, dev = _render_both({"timestamp": "t", "level": "warning", "event": "evt", "reason": "line1\nline2"})
  assert lean == dev
  assert "reason='line1\\nline2'" in lean


def test_no_fields_rstrips() -> None:
  lean, dev = _render_both({"timestamp": "t", "level": "info", "event": "e"})
  assert lean == dev
  assert lean == "t [info     ] e"


def test_missing_timestamp_or_level_falls_back_to_dev() -> None:
  lean, dev = _render_both({"event": "only-event"})
  assert lean == dev
  lean, dev = _render_both({"timestamp": "t", "event": "no-level"})
  assert lean == dev


def test_exception_and_stack_and_logger_keys_fall_back_to_dev() -> None:
  for extra in ({"exc_info": True}, {"exception": "boom"}, {"stack": "frame1\nframe2"}, {"logger": "server"},
                {"logger_name": "server"}):
    event_dict = {"timestamp": "t", "level": "error", "event": "evt", **extra}
    lean, dev = _render_both(event_dict)
    assert lean == dev


def test_color_mode_delegates_every_line_to_dev() -> None:
  dev = ConsoleRenderer(colors=True)
  lean = _LeanLineRenderer()
  lean._colors = True
  event_dict = {"timestamp": "t", "level": "info", "event": "evt", "k": 1}
  assert lean(None, "info", dict(event_dict)) == dev(None, "info", dict(event_dict))


def test_configured_renderer_serves_the_lean_path() -> None:
  from src.core.log_once import ensure_lean_renderer

  ensure_lean_renderer()
  sink = io.StringIO()
  with contextlib.redirect_stdout(sink):
    structlog.get_logger().info("http_request", status=200, path="/x")
  line = sink.getvalue()
  assert line.startswith("20") and "[info     ] http_request" in line
  assert "path=/x status=200" in line
  dev = ConsoleRenderer(colors=False)
  event_dict = {"level": "info", "event": "http_request", "status": 200, "path": "/x"}
  # The timestamp prefix is the same TimeStamper format in both chains; the
  # comparison starts at the level column, the first renderer-shaped part.
  # (PrintLogger appends the newline the renderer return value does not carry.)
  dev_line = dev(None, "info", dict(event_dict))
  assert line[line.index("["):].rstrip("\n") == dev_line[dev_line.index("["):]


def test_configured_chain_stamps_local_time_like_the_default_chain() -> None:
  from datetime import datetime, timedelta

  from src.core.log_once import _LocalStampProcessor, ensure_lean_renderer

  ensure_lean_renderer()
  stamps = [p for p in structlog.get_config()["processors"] if isinstance(p, _LocalStampProcessor)]
  assert len(stamps) == 1
  # TimeStamper's utc=True default would shift every served stamp to UTC; the
  # processor's stamp must read local wall time like the default chain's
  # TimeStamper(utc=False) does. The stamps can straddle a second boundary,
  # so the clocks compare within one second instead of byte-for-byte.
  fmt = "%Y-%m-%d %H:%M:%S"
  stamped = stamps[0](None, "info", {"event": "x"})["timestamp"]
  reference = structlog.processors.TimeStamper(fmt=fmt, utc=False)(None, "info", {})["timestamp"]
  assert abs(datetime.strptime(stamped, fmt) - datetime.strptime(reference, fmt)) <= timedelta(seconds=1)


@pytest.mark.parametrize(
    ("env", "tty", "expected"), [
        ({}, False, False),
        ({}, True, True),
        ({
            "NO_COLOR": "1"
        }, True, False),
        ({
            "FORCE_COLOR": "1"
        }, False, True),
    ])
def test_color_decision_mirrors_the_default_chain(
    env: dict, tty: bool, expected: bool, monkeypatch: pytest.MonkeyPatch) -> None:
  # structlog._config decides at import: not NO_COLOR and (FORCE_COLOR or
  # isatty); the four shapes below are that rule (_has_colors is always true
  # on linux). ConsoleRenderer only re-decides when colors=None, which it
  # treats as _has_colors at its own import — so the table is the reference.
  for key in ("NO_COLOR", "FORCE_COLOR"):
    monkeypatch.delenv(key, raising=False)
  for key, value in env.items():
    monkeypatch.setenv(key, value)
  monkeypatch.setattr(sys.stdout, "isatty", lambda: tty, raising=False)
  from src.core.log_once import _LeanLineRenderer as lean_cls

  assert lean_cls()._colors == expected


# ---------------------------------------------------------------------------
# The composed access line (log_http_request_line)
# ---------------------------------------------------------------------------


def _composed_line(
    method: object, path: object, status: object, duration_ms: object, client: object, error: str | None = None) -> str:
  sink = io.StringIO()
  with contextlib.redirect_stdout(sink):
    log_http_request_line(method=method, path=path, status=status, duration_ms=duration_ms, client=client, error=error)
  return sink.getvalue()


@pytest.mark.parametrize(
    ("method", "path", "status", "duration_ms", "client", "error"), [
        ("GET", "/api/sessions/status", 401, 1, "127.0.0.1", None),
        ("POST", "/api/chat/s1/cancel", 200, 12, "100.84.122.38", None),
        ("GET", "/x", 500, 3, "-", "ModuleNotFoundError"),
        ("GET", "/path with spaces", 404, 0, "client=odd", None),
        ("GET", '/path"quoted', 200, 5, "ümläut-✓", None),
        ("GET", "/api/x", None, 0, "t", None),
        ("GET", "/api/x", 204, 3.5, "t", "e=mc^2"),
    ],
    ids=["401", "200", "error", "spaces", "quotes-unicode", "status-none", "float-duration"])
def test_composed_access_line_matches_the_lean_renderer(
    method: object, path: object, status: object, duration_ms: object, client: object, error: str | None) -> None:
  line = _composed_line(method, path, status, duration_ms, client, error)
  assert line.endswith("\n")
  timestamp = line[:19]
  event_dict = {
      "timestamp": timestamp,
      "level": "info",
      "event": "http_request",
      "client": client,
      "duration_ms": duration_ms,
      "method": method,
      "path": path,
      "status": status,
  }
  if error is not None:
    event_dict["error"] = error
  lean = _LeanLineRenderer()
  assert line.rstrip("\n") == lean(None, "info", dict(event_dict))


def test_composed_access_line_writes_once() -> None:
  writes: list[str] = []

  class _Recorder:

    def write(self, s: str) -> int:
      writes.append(s)
      return len(s)

  with contextlib.redirect_stdout(_Recorder()):
    log_http_request_line(method="GET", path="/x", status=200, duration_ms=1, client="t")
  assert len(writes) == 1
  assert writes[0].endswith("\n")
  assert not writes[0].endswith("\n\n")


# ---------------------------------------------------------------------------
# The per-minute stamp prefix memo
# ---------------------------------------------------------------------------


def _stamp_struct(year: int, month: int, day: int, hour: int, minute: int, second: int) -> time.struct_time:
  return time.struct_time((year, month, day, hour, minute, second, 0, 1, -1))


@pytest.fixture
def _stamp_memo_isolated(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(log_once, "_stamp_prefix_memo", ((0, 0, 0, 0, 0), ""))


def test_stamp_prefix_reused_within_a_minute(_stamp_memo_isolated: None, monkeypatch: pytest.MonkeyPatch) -> None:
  current = _stamp_struct(2026, 9, 24, 15, 1, 7)
  monkeypatch.setattr(log_once.time, "localtime", lambda: current)
  assert _local_timestamp() == "2026-09-24 15:01:07"
  current = _stamp_struct(2026, 9, 24, 15, 1, 59)
  assert _local_timestamp() == "2026-09-24 15:01:59"


def test_stamp_prefix_rebuilt_on_minute_rollover(_stamp_memo_isolated: None, monkeypatch: pytest.MonkeyPatch) -> None:
  current = _stamp_struct(2026, 9, 24, 15, 1, 59)
  monkeypatch.setattr(log_once.time, "localtime", lambda: current)
  assert _local_timestamp() == "2026-09-24 15:01:59"
  current = _stamp_struct(2026, 9, 24, 15, 2, 0)
  assert _local_timestamp() == "2026-09-24 15:02:00"


def test_stamp_survives_a_stale_minute_after_rollover(
    _stamp_memo_isolated: None, monkeypatch: pytest.MonkeyPatch) -> None:
  """A reader whose minute lost the memo race still renders its own minute."""
  current = _stamp_struct(2026, 9, 24, 15, 1, 7)
  monkeypatch.setattr(log_once.time, "localtime", lambda: current)
  assert _local_timestamp() == "2026-09-24 15:01:07"
  current = _stamp_struct(2026, 9, 24, 15, 2, 30)
  assert _local_timestamp() == "2026-09-24 15:02:30"
  current = _stamp_struct(2026, 9, 24, 15, 1, 42)
  assert _local_timestamp() == "2026-09-24 15:01:42"
