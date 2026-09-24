"""The bare-file arm's gzip memo (src/api/files.py): repeat serves answer stored bytes."""

from pathlib import Path

import pytest
from conftest import assert_gzip_served, gzip_explode_compress, mount_production_gzip
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import files as files_api
from src.api import responses as responses_api


def _bare_app() -> FastAPI:
  """The files router alone, no gzip middleware: route-side contracts only."""
  app = FastAPI()
  app.include_router(files_api.router, prefix="/absolute_filepath")
  return app


def _build_client() -> TestClient:
  """The files router behind the production gzip mount, so the test sees the
  skip the pre-compressed response buys."""
  app = _bare_app()
  mount_production_gzip(app)
  return TestClient(app)


def _write_page(tmp_path: Path) -> Path:
  path = tmp_path / "page.html"
  path.write_text("<html><body><h1>Plan</h1></body></html>", encoding="utf-8")
  return path


def test_repeat_serve_answers_from_stored_gzip(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A repeat gzip serve of an unchanged file must serve the stored compressed
  body with zero deflate calls — the middleware's per-request per-chunk pass is
  what the memo removes."""
  page = _write_page(tmp_path)
  url = "/absolute_filepath" + str(page)
  client = _build_client()
  first = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert first.status_code == 200
  assert_gzip_served(first)
  assert first.content == page.read_bytes()

  monkeypatch.setattr(responses_api, "gzip_level1", gzip_explode_compress("repeat file serve re-ran the deflate"))
  second = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert second.status_code == 200
  assert_gzip_served(second)
  assert second.content == first.content


def test_rewritten_file_serves_fresh_bytes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A rewrite moves the (mtime_ns, size) signature the memo keys on — the
  compressed form must never serve bytes it was not built from."""
  page = _write_page(tmp_path)
  url = "/absolute_filepath" + str(page)
  client = _build_client()
  assert client.get(url, headers={"Accept-Encoding": "gzip"}).content == page.read_bytes()

  page.write_text("<html><body><h1>Rewritten</h1></body></html>", encoding="utf-8")
  resp = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert_gzip_served(resp)
  assert resp.content == page.read_bytes()


def test_skip_listed_media_never_enters_the_memo_arm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A media type outside the text gate must never leave the route as a
  pre-compressed body: unknown and binary formats keep the streaming arm's
  identity contract (wire == raw bytes)."""
  monkeypatch.setattr(responses_api, "gzip_level1", gzip_explode_compress("unlisted media re-ran the deflate"))
  blob = tmp_path / "img.png"
  blob.write_bytes(b"\x89PNG\r\n\x1a\n" + b"0" * 2000)
  resp = TestClient(_bare_app()).get("/absolute_filepath" + str(blob), headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert "content-encoding" not in resp.headers
  assert resp.content == blob.read_bytes()


def test_json_file_rides_the_memo_arm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """application/json sits in the gate's named set: a trace-sized repeat serve
  answers from the stored gzip form with zero deflate calls."""
  blob = tmp_path / "trace.json"
  blob.write_text("[" + ",".join(f'{{"ph": "X", "ts": {i}}}' for i in range(2000)) + "]", encoding="utf-8")
  url = "/absolute_filepath" + str(blob)
  client = _build_client()
  first = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert first.status_code == 200
  assert_gzip_served(first)
  assert first.content == blob.read_bytes()
  monkeypatch.setattr(responses_api, "gzip_level1", gzip_explode_compress("repeat json serve re-ran the deflate"))
  second = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert second.status_code == 200
  assert second.content == first.content


def test_range_request_stays_on_streaming_arm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Range request never takes the memo arm: byte ranges address the raw
  representation, so the request rides the streaming FileResponse unchanged."""
  page = _write_page(tmp_path)
  monkeypatch.setattr(responses_api, "gzip_level1", gzip_explode_compress("range serve re-ran the deflate"))
  resp = _build_client().get(
      "/absolute_filepath" + str(page), headers={
          "Accept-Encoding": "gzip",
          "Range": "bytes=0-9"
      })
  assert resp.status_code == 206
  assert resp.content == page.read_bytes()[:10]


def test_no_gzip_accept_gets_raw_bytes(tmp_path: Path) -> None:
  """A client whose Accept-Encoding names no gzip reads the raw file bytes, no
  encoding set — the negotiation gate the memo arm sits behind."""
  page = _write_page(tmp_path)
  resp = _build_client().get("/absolute_filepath" + str(page), headers={"Accept-Encoding": "br"})
  assert resp.status_code == 200
  assert "content-encoding" not in resp.headers
  assert resp.content == page.read_bytes()


def test_oversize_file_stays_on_streaming_arm(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A file over the memo cap must not enter the memo arm: a whole-body read
  plus its gzip form would hold both resident for a file the streaming arm
  serves chunk-wise without buffering."""
  monkeypatch.setattr(files_api, "_SERVED_FILE_GZIP_MAX_BYTES", 8)
  page = _write_page(tmp_path)
  monkeypatch.setattr(responses_api, "gzip_level1", gzip_explode_compress("oversize serve re-ran the deflate"))
  resp = _build_client().get("/absolute_filepath" + str(page), headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert "content-encoding" not in resp.headers
  assert resp.content == page.read_bytes()
