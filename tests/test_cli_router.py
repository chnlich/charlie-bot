"""Tests for ``charliebot router``: the stdin/stdout contract, router.yaml freshness and Jev's retry rules.

A fake Jev serves each case on a free 127.0.0.1 port; every host name and backend id here is synthetic.
"""

import io
import json
import pathlib
import sys
import threading
import types
from collections.abc import Iterator
from http import server as http_server

import pytest
import yaml

from src.features.router import cli, routing
from src.infra import timeouts

_INSTRUCTIONS = "Which kind of work is this?"
_ROUTINE_RULE = "Routine work: fixes, tests and short actions."
_DESIGN_RULE = "Design work: a new mechanism across modules."
_STRONG = ("strong-alpha", "strong-beta")
_POOL = "pool-gamma"
_API_KEY = "test-key"


class _FakeJev:
  """A Jev endpoint that records each request body and answers from a scripted reply list."""

  def __init__(self) -> None:
    self.requests: list[dict] = []
    self.authorizations: list[str] = []
    self.replies: list[tuple[int, dict]] = []
    self._httpd: http_server.HTTPServer | None = None

  def start(self) -> None:
    fake = self

    class Handler(http_server.BaseHTTPRequestHandler):

      def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        fake.requests.append(json.loads(self.rfile.read(length)))
        fake.authorizations.append(self.headers.get("Authorization", ""))
        status, payload = fake.replies.pop(0)
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

      def log_message(self, format: str, *args: object) -> None:  # noqa: A002  stdlib signature mirror
        pass

    self._httpd = http_server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=self._httpd.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True).start()

  @property
  def url(self) -> str:
    return f"http://127.0.0.1:{self._httpd.server_address[1]}/v1/systemone"

  def stop(self) -> None:
    self._httpd.shutdown()
    self._httpd.server_close()


@pytest.fixture
def jev() -> Iterator[_FakeJev]:
  fake = _FakeJev()
  fake.start()
  yield fake
  fake.stop()


def _answer(design: float) -> tuple[int, dict]:
  """One Jev choice answer carrying *design* as the design probability."""
  return 200, {"answers": {"kind": {"choice": "design", "probabilities": {"routine": 1 - design, "design": design}}}}


def _write_router(home: pathlib.Path, url: str, *, strong: tuple[str, ...] = _STRONG) -> pathlib.Path:
  path = home / routing.ROUTER_FILENAME
  path.write_text(
      yaml.safe_dump(
          {
              "jev": {
                  "url": url,
                  "model": "jev-test",
                  "credential": "jev",
              },
              "instructions": _INSTRUCTIONS,
              "routine": _ROUTINE_RULE,
              "design": _DESIGN_RULE,
              "strong": list(strong),
          }),
      encoding="utf-8")
  return path


def _write_credentials(home: pathlib.Path, *, section: str = "jev") -> None:
  home.mkdir(parents=True, exist_ok=True)
  (home / "credentials.yaml").write_text(yaml.safe_dump({section: {"api_key": _API_KEY}}), encoding="utf-8")


def _run(monkeypatch: pytest.MonkeyPatch, payload: str | dict) -> None:
  """Run the CLI once: *payload* is the whole stdin text."""
  text = payload if isinstance(payload, str) else json.dumps(payload)
  monkeypatch.setattr(sys, "stdin", io.StringIO(text))
  monkeypatch.setattr(sys, "argv", ["charliebot router"])
  cli.main()


def _request(
    role: str = "implement", text: str = "Add a test for the parser.", candidates: tuple[str, ...] = ()) -> dict:
  return {
      "role": role,
      "text": text,
      "candidates": [{
          "id": identifier,
          "cooling_until": None
      } for identifier in candidates],
  }


def _stdout_json(capsys: pytest.CaptureFixture[str]) -> dict:
  captured = capsys.readouterr()
  assert captured.err == ""
  return json.loads(captured.out)


def test_design_work_returns_the_strong_backends_that_are_candidates_in_strong_order(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  _write_router(profile_home, jev.url)
  jev.replies.append(_answer(0.86))

  _run(monkeypatch, _request(text="Design a new caching layer.", candidates=(_STRONG[1], _POOL, _STRONG[0])))

  assert _stdout_json(capsys) == {"backends": ["strong-alpha", "strong-beta"], "reason": "design p=0.86"}
  body = jev.requests[0]
  assert body["model"] == "jev-test"
  assert body["state"] == "Design a new caching layer."
  assert body["questions"]["kind"]["type"] == "choice"
  assert body["questions"]["kind"]["instructions"] == _INSTRUCTIONS
  assert body["questions"]["kind"]["criteria"] == {"routine": _ROUTINE_RULE, "design": _DESIGN_RULE}
  assert jev.authorizations == [f"Bearer {_API_KEY}"]


def test_routine_work_returns_no_backends(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  _write_router(profile_home, jev.url)
  jev.replies.append(_answer(0.12))

  _run(monkeypatch, _request(candidates=(_STRONG[0], _POOL)))

  assert _stdout_json(capsys) == {"backends": [], "reason": "routine p=0.12"}


def test_one_rate_limit_waits_then_returns_the_answer(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  _write_router(profile_home, jev.url)
  jev.replies += [(429, {"error": "rate limited"}), _answer(0.86)]
  pauses: list[float] = []
  monkeypatch.setattr(routing, "time", types.SimpleNamespace(sleep=pauses.append))

  _run(monkeypatch, _request(candidates=_STRONG))

  assert _stdout_json(capsys) == {"backends": ["strong-alpha", "strong-beta"], "reason": "design p=0.86"}
  assert len(jev.requests) == 2
  assert pauses == [timeouts.ROUTER_JEV_RATE_LIMIT_WAIT]


def test_two_rate_limits_exit_non_zero_with_the_status_and_body(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  _write_router(profile_home, jev.url)
  jev.replies += [(429, {"error": "rate limited"}), (429, {"error": "still limited"})]
  monkeypatch.setattr(routing, "time", types.SimpleNamespace(sleep=lambda seconds: None))

  with pytest.raises(SystemExit) as exit_info:
    _run(monkeypatch, _request(candidates=_STRONG))

  assert exit_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert "429" in captured.err
  assert "still limited" in captured.err


def test_another_http_error_exits_non_zero_without_printing_an_answer(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  _write_router(profile_home, jev.url)
  jev.replies.append((500, {"error": "gateway down"}))

  with pytest.raises(SystemExit) as exit_info:
    _run(monkeypatch, _request(candidates=_STRONG))

  assert exit_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert "500" in captured.err
  assert "gateway down" in captured.err


def test_an_answer_without_a_design_probability_exits_non_zero(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  _write_router(profile_home, jev.url)
  jev.replies.append((200, {"answers": {"other": {}}}))

  with pytest.raises(SystemExit) as exit_info:
    _run(monkeypatch, _request(candidates=_STRONG))

  assert exit_info.value.code == 1
  assert capsys.readouterr().out == ""


def test_a_thread_text_is_cut_to_its_last_twenty_thousand_characters(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  _write_router(profile_home, jev.url)
  long_text = "x" * 25_000 + "THREAD-END"
  jev.replies += [_answer(0.12), _answer(0.12)]

  _run(monkeypatch, _request(role="thread", text=long_text, candidates=_STRONG))
  _run(monkeypatch, _request(role="implement", text=long_text, candidates=_STRONG))

  capsys.readouterr()
  assert jev.requests[0]["state"] == long_text[-20_000:]
  assert len(jev.requests[0]["state"]) == 20_000
  assert jev.requests[1]["state"] == long_text


def test_an_edit_to_router_yaml_takes_effect_on_the_next_call(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  _write_router(profile_home, jev.url, strong=("strong-alpha",))
  jev.replies += [_answer(0.86), _answer(0.86)]

  _run(monkeypatch, _request(candidates=(_STRONG[0], _STRONG[1])))
  assert _stdout_json(capsys)["backends"] == ["strong-alpha"]

  _write_router(profile_home, jev.url, strong=("strong-beta",))
  _run(monkeypatch, _request(candidates=(_STRONG[0], _STRONG[1])))
  assert _stdout_json(capsys)["backends"] == ["strong-beta"]


def test_a_malformed_router_yaml_exits_non_zero_naming_the_file_and_field(
    profile_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  path = profile_home / routing.ROUTER_FILENAME
  path.write_text(
      yaml.safe_dump({
          "jev": {
              "model": "jev-test",
              "credential": "jev"
          },
          "strong": ["strong-alpha"]
      }), encoding="utf-8")

  with pytest.raises(SystemExit) as exit_info:
    _run(monkeypatch, _request(candidates=_STRONG))

  assert exit_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert captured.err.startswith("router: ")
  assert str(path) in captured.err
  assert "url" in captured.err


def test_router_yaml_that_is_not_yaml_exits_non_zero_naming_the_file(
    profile_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home)
  path = profile_home / routing.ROUTER_FILENAME
  path.write_text("jev: [unclosed\n", encoding="utf-8")

  with pytest.raises(SystemExit) as exit_info:
    _run(monkeypatch, _request(candidates=_STRONG))

  assert exit_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert captured.err.startswith("router: ")
  assert str(path) in captured.err


def test_a_missing_router_yaml_exits_non_zero_naming_the_file(
    profile_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  with pytest.raises(SystemExit) as exit_info:
    _run(monkeypatch, _request(candidates=_STRONG))

  assert exit_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert str(profile_home / routing.ROUTER_FILENAME) in captured.err


def test_a_missing_jev_key_exits_non_zero_naming_the_credentials_file(
    profile_home: pathlib.Path, jev: _FakeJev, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str]) -> None:
  _write_credentials(profile_home, section="another-section")
  _write_router(profile_home, jev.url)

  with pytest.raises(SystemExit) as exit_info:
    _run(monkeypatch, _request(candidates=_STRONG))

  assert exit_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert "credentials.yaml" in captured.err
  assert "jev.api_key" in captured.err
  assert not jev.requests


def test_the_worst_case_stays_inside_the_callers_twenty_five_seconds() -> None:
  worst_case = 2 * timeouts.ROUTER_JEV_REQUEST_TIMEOUT + timeouts.ROUTER_JEV_RATE_LIMIT_WAIT
  assert worst_case < 25
  assert timeouts.ROUTER_JEV_REQUEST_TIMEOUT == 5
