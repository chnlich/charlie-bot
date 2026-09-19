"""_LeanLineRenderer tests: byte-identical output against the dev ConsoleRenderer.

Every test drives both renderers over the same event dict and asserts the strings
are equal, so a structlog upgrade that moves a padding or quoting rule fails here.
"""

import contextlib
import io
import sys

import pytest
import structlog
from structlog.dev import ConsoleRenderer

from src.core.log_once import _LeanLineRenderer


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
  from src.core.log_once import ensure_lean_renderer

  ensure_lean_renderer()
  stampers = [p for p in structlog.get_config()["processors"] if isinstance(p, structlog.processors.TimeStamper)]
  assert len(stampers) == 1
  # TimeStamper defaults to utc=True; the default chain passes utc=False, and
  # dropping it shifts every served stamp to UTC.
  assert stampers[0].fmt == "%Y-%m-%d %H:%M:%S" and stampers[0].utc is False


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
