import gzip
import io
import json
import os
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

import pytest
from fastapi import FastAPI, HTTPException
from fastapi.testclient import TestClient
from starlette.requests import Request

import server
from src.api import pages
from src.core.trace_merge import merge_traces as real_merge_traces


def _write_trace(path: Path, marker: str = "event") -> None:
  path.write_text(
      json.dumps({"traceEvents": [{"ph": "X", "pid": 1, "tid": 1, "name": marker}]}),
      encoding="utf-8",
  )


def _request() -> Request:
  return Request({
      "type": "http",
      "method": "GET",
      "path": "/perfetto",
      "headers": [],
      "query_string": b"",
      "scheme": "http",
      "server": ("testserver", 80),
      "client": ("127.0.0.1", 12345),
  })


@pytest.fixture
def client(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> TestClient:
  monkeypatch.setattr(pages, "_PERFETTO_MERGE_CACHE_DIR", tmp_path / "cache")
  app = FastAPI()
  app.add_middleware(server._CharlieBotGZipMiddleware, minimum_size=1)
  app.include_router(pages.router)
  return TestClient(app)


def test_merge_endpoint_errors_and_gzip_response(client: TestClient, tmp_path: Path) -> None:
  assert client.get("/perfetto/merged").status_code == 400
  assert client.get("/perfetto/merged", params={"dir": str(tmp_path / "empty")}).status_code == 400

  missing = tmp_path / "missing.json"
  missing_response = client.get("/perfetto/merged", params={"trace": str(missing)})
  assert missing_response.status_code == 404
  assert str(missing) in missing_response.json()["detail"]

  binary = tmp_path / "trace.pftrace"
  binary.write_bytes(b"not json")
  assert client.get("/perfetto/merged", params={"trace": str(binary)}).status_code == 400

  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  response = client.get("/perfetto/merged", params={"trace": str(trace)})
  assert response.status_code == 200
  assert response.content[:2] == b"\x1f\x8b"
  assert response.headers["content-type"] == "application/gzip"
  assert "content-encoding" not in response.headers


def test_merge_endpoint_surfaces_corrupt_json_error(client: TestClient, tmp_path: Path) -> None:
  corrupt = tmp_path / "corrupt.json"
  corrupt.write_text("{broken", encoding="utf-8")
  response = client.get("/perfetto/merged", params={"trace": str(corrupt)})
  assert response.status_code == 500
  assert "Expecting property name" in response.json()["detail"]


def test_merge_endpoint_combines_trace_then_directory_inputs(client: TestClient, tmp_path: Path) -> None:
  explicit = tmp_path / "rank0.json"
  directory = tmp_path / "ranks"
  directory.mkdir()
  discovered = directory / "rank1.json"
  _write_trace(explicit, "first")
  _write_trace(discovered, "second")

  response = client.get(
      "/perfetto/merged",
      params=[("trace", str(explicit)), ("dir", str(directory)), ("pattern", "rank*.json")],
  )
  with gzip.GzipFile(fileobj=io.BytesIO(response.content)) as merged_file:
    events = json.load(merged_file)["traceEvents"]
  names = [event["name"] for event in events if event.get("ph") == "X"]
  assert names == ["first", "second"]


def test_merge_cache_hits_invalidates_on_mtime_and_prunes(
    client: TestClient,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  calls = 0

  def counting_merge(paths: list[Path], out_path: Path, slim: bool) -> None:
    nonlocal calls
    calls += 1
    real_merge_traces(paths, out_path, slim)

  monkeypatch.setattr(pages, "merge_traces", counting_merge)
  first = tmp_path / "rank0.json"
  _write_trace(first)

  assert client.get("/perfetto/merged", params={"trace": str(first)}).status_code == 200
  assert client.get("/perfetto/merged", params={"trace": str(first)}).status_code == 200
  assert calls == 1

  stat = first.stat()
  os.utime(first, ns=(stat.st_atime_ns, stat.st_mtime_ns + 1_000_000))
  assert client.get("/perfetto/merged", params={"trace": str(first)}).status_code == 200
  assert calls == 2

  for index in range(1, 9):
    trace = tmp_path / f"rank{index}.json"
    _write_trace(trace, str(index))
    assert client.get("/perfetto/merged", params={"trace": str(trace)}).status_code == 200
  assert len(list(pages._PERFETTO_MERGE_CACHE_DIR.glob("*.json.gz"))) <= 8


@pytest.mark.asyncio
async def test_perfetto_page_single_trace_passthrough(tmp_path: Path) -> None:
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  response = await pages.perfetto_viewer(
      _request(), trace=[str(trace)], dir=None, pattern="*.json", title=None, slim=None)
  assert response.context["trace_url"] == f"/files{trace}"
  assert response.context["warn"] is None


@pytest.mark.asyncio
async def test_perfetto_page_multiple_local_json_uses_one_merged_url(tmp_path: Path) -> None:
  traces = [tmp_path / "rank0.json", tmp_path / "rank1.json"]
  for trace in traces:
    _write_trace(trace)
  response = await pages.perfetto_viewer(
      _request(), trace=[str(path) for path in traces], dir=None, pattern="*.json", title="Ranks", slim=1)

  merged_url = response.context["trace_url"]
  query = parse_qs(urlsplit(merged_url).query)
  assert urlsplit(merged_url).path == "/perfetto/merged"
  assert query["trace"] == [str(path) for path in traces]
  assert query["slim"] == ["1"]
  assert "traceUrls" not in response.body.decode("utf-8")


@pytest.mark.asyncio
async def test_perfetto_page_directory_forwards_discovery_params(tmp_path: Path) -> None:
  trace_dir = tmp_path / "ranks"
  trace_dir.mkdir()
  _write_trace(trace_dir / "rank1.json")
  _write_trace(trace_dir / "rank0.json")
  response = await pages.perfetto_viewer(
      _request(), trace=[], dir=str(trace_dir), pattern="rank*.json", title=None, slim=None)

  query = parse_qs(urlsplit(response.context["trace_url"]).query)
  assert query == {"dir": [str(trace_dir)], "pattern": ["rank*.json"]}


@pytest.mark.asyncio
async def test_perfetto_page_mixed_inputs_warns_and_uses_first(tmp_path: Path) -> None:
  local = tmp_path / "rank0.json"
  _write_trace(local)
  response = await pages.perfetto_viewer(
      _request(),
      trace=[str(local), "https://example.com/rank1.json"],
      dir=None,
      pattern="*.json",
      title=None,
      slim=None,
  )
  assert response.context["trace_url"] == f"/files{local}"
  assert "showing first trace only" in response.context["warn"]


@pytest.mark.asyncio
async def test_perfetto_page_rejects_empty_input() -> None:
  with pytest.raises(HTTPException) as error:
    await pages.perfetto_viewer(_request(), trace=[], dir=None, pattern="*.json", title=None, slim=None)
  assert error.value.status_code == 400


def test_perfetto_template_has_no_browser_merger() -> None:
  template = Path("web/templates/perfetto.html").read_text(encoding="utf-8")
  for deleted_name in ("mergeJsonTraces", "fetchAllTraces", "isJsonTrace", "traceUrls", "traceNames"):
    assert deleted_name not in template
