import asyncio
import gzip
import io
import json
import os
import threading
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
  cache_dir = tmp_path / "cache"
  monkeypatch.setattr(pages, "_perfetto_merge_cache_dir", lambda: cache_dir)
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


def test_direct_pass_gzip_decompresses_to_identical_bytes(client: TestClient, tmp_path: Path) -> None:
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  response = client.get("/perfetto/merged", params={"trace": str(trace)})
  assert response.status_code == 200
  assert response.headers["content-type"] == "application/gzip"
  assert "content-encoding" not in response.headers
  assert gzip.decompress(response.content) == trace.read_bytes()


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
  merge_calls = 0

  def counting_merge(paths: list[Path], out_path: Path, slim: bool) -> None:
    nonlocal merge_calls
    merge_calls += 1
    real_merge_traces(paths, out_path, slim)

  monkeypatch.setattr(pages, "merge_traces", counting_merge)
  monkeypatch.setattr(pages, "_merge_executor", lambda: None)

  # Merge leg: multiple inputs still go through merge_traces.
  first = tmp_path / "rank0.json"
  second = tmp_path / "rank1.json"
  _write_trace(first, "first")
  _write_trace(second, "second")
  merge_params = [("trace", str(first)), ("trace", str(second))]

  assert client.get("/perfetto/merged", params=merge_params).status_code == 200
  assert client.get("/perfetto/merged", params=merge_params).status_code == 200
  assert merge_calls == 1

  stat = first.stat()
  os.utime(first, ns=(stat.st_atime_ns, stat.st_mtime_ns + 1_000_000))
  assert client.get("/perfetto/merged", params=merge_params).status_code == 200
  assert merge_calls == 2

  # Direct-pass leg: a single input bypasses merge_traces and builds via gzip.
  direct_calls = 0
  real_build_direct_pass_gzip = pages._build_direct_pass_gzip

  def counting_direct_pass(path: Path, out_path: Path) -> None:
    nonlocal direct_calls
    direct_calls += 1
    real_build_direct_pass_gzip(path, out_path)

  monkeypatch.setattr(pages, "_build_direct_pass_gzip", counting_direct_pass)

  solo = tmp_path / "solo.json"
  _write_trace(solo)

  assert client.get("/perfetto/merged", params={"trace": str(solo)}).status_code == 200
  assert client.get("/perfetto/merged", params={"trace": str(solo)}).status_code == 200
  assert direct_calls == 1
  assert merge_calls == 2

  stat = solo.stat()
  os.utime(solo, ns=(stat.st_atime_ns, stat.st_mtime_ns + 1_000_000))
  assert client.get("/perfetto/merged", params={"trace": str(solo)}).status_code == 200
  assert direct_calls == 2

  for index in range(pages._PERFETTO_MERGE_CACHE_LIMIT + 5):
    trace = tmp_path / f"prune{index}.json"
    _write_trace(trace, str(index))
    assert client.get("/perfetto/merged", params={"trace": str(trace)}).status_code == 200
  assert len(list(pages._perfetto_merge_cache_dir().glob("*.json.gz"))) <= pages._PERFETTO_MERGE_CACHE_LIMIT


@pytest.mark.asyncio
async def test_perfetto_page_single_local_trace_uses_merged_url(tmp_path: Path) -> None:
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  response = await pages.perfetto_viewer(
      _request(), trace=[str(trace)], dir=None, pattern="*.json", title=None, slim=None)

  merged_url = response.context["trace_url"]
  assert urlsplit(merged_url).path == "/perfetto/merged"
  assert parse_qs(urlsplit(merged_url).query)["trace"] == [str(trace)]
  assert response.context["warn"] is None


@pytest.mark.asyncio
async def test_perfetto_page_single_remote_trace_has_no_warning() -> None:
  response = await pages.perfetto_viewer(
      _request(),
      trace=["https://example.com/rank0.json"],
      dir=None,
      pattern="*.json",
      title=None,
      slim=None,
  )
  assert response.context["trace_url"] == "https://example.com/rank0.json"
  assert response.context["warn"] is None


@pytest.mark.asyncio
async def test_perfetto_page_directory_single_match_uses_merged_url(tmp_path: Path) -> None:
  trace_dir = tmp_path / "ranks"
  trace_dir.mkdir()
  _write_trace(trace_dir / "rank0.json")
  response = await pages.perfetto_viewer(
      _request(), trace=[], dir=str(trace_dir), pattern="rank*.json", title=None, slim=None)

  assert urlsplit(response.context["trace_url"]).path == "/perfetto/merged"
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


async def _wait_until(predicate, timeout: float = 5.0) -> bool:
  loop = asyncio.get_running_loop()
  deadline = loop.time() + timeout
  while loop.time() < deadline:
    if predicate():
      return True
    await asyncio.sleep(0.01)
  return predicate()


def test_slim_query_accepts_booleans_and_rejects_other_values(client: TestClient, tmp_path: Path) -> None:
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  for value in ("1", "true", "0", "false"):
    assert client.get("/perfetto/merged", params={"trace": str(trace), "slim": value}).status_code == 200
  assert client.get("/perfetto/merged", params={"trace": str(trace), "slim": "2"}).status_code == 422

  # slim is forwarded as a boolean on the viewer page and merged URL.
  assert client.get("/perfetto", params={"trace": str(trace), "slim": "1"}).status_code == 200
  assert client.get("/perfetto", params={"trace": str(trace), "slim": "true"}).status_code == 200
  assert client.get("/perfetto", params={"trace": str(trace), "slim": "0"}).status_code == 200
  assert client.get("/perfetto", params={"trace": str(trace), "slim": "2"}).status_code == 422


def test_cache_eviction_follows_last_use(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cache_dir = tmp_path / "cache"
  monkeypatch.setattr(pages, "_perfetto_merge_cache_dir", lambda: cache_dir)
  monkeypatch.setattr(pages, "_merge_executor", lambda: None)
  app = FastAPI()
  app.include_router(pages.router)
  client = TestClient(app)

  kept = tmp_path / "kept.json"
  _write_trace(kept)
  assert client.get("/perfetto/merged", params={"trace": str(kept)}).status_code == 200
  kept_path = next(pages._perfetto_merge_cache_dir().glob("*.json.gz"))

  # Fill the cache to the limit.
  for index in range(pages._PERFETTO_MERGE_CACHE_LIMIT - 1):
    trace = tmp_path / f"fill{index}.json"
    _write_trace(trace)
    assert client.get("/perfetto/merged", params={"trace": str(trace)}).status_code == 200
  assert kept_path.is_file()

  # A hit refreshes the entry's mtime to the newest, protecting it from eviction.
  old_mtime = kept_path.stat().st_mtime_ns
  assert client.get("/perfetto/merged", params={"trace": str(kept)}).status_code == 200
  assert kept_path.stat().st_mtime_ns > old_mtime

  # Build enough newer entries to exceed the limit; eviction follows last use, so the
  # recently-hit entry survives while older ones are evicted.
  for index in range(5):
    trace = tmp_path / f"newer{index}.json"
    _write_trace(trace)
    assert client.get("/perfetto/merged", params={"trace": str(trace)}).status_code == 200
  assert kept_path.is_file()
  assert len(list(pages._perfetto_merge_cache_dir().glob("*.json.gz"))) <= pages._PERFETTO_MERGE_CACHE_LIMIT


@pytest.mark.asyncio
async def test_two_phase_status_strings_are_present(tmp_path: Path) -> None:
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  response = await pages.perfetto_viewer(
      _request(), trace=[str(trace)], dir=None, pattern="*.json", title=None, slim=None)
  body = response.body.decode("utf-8")
  # Merge phase: a seconds counter is ticking while the server merges.
  assert "Merging" in body and "on the server" in body
  assert "on the server" in body and "s`" in body
  # Download phase: progress is driven by Content-Length.
  assert "Downloading" in body
  assert "Content-Length" in body


def test_single_flight_one_build_per_key(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cache_dir = tmp_path / "cache"
  monkeypatch.setattr(pages, "_perfetto_merge_cache_dir", lambda: cache_dir)
  monkeypatch.setattr(pages, "_merge_executor", lambda: None)
  first = tmp_path / "rank0.json"
  second = tmp_path / "rank1.json"
  _write_trace(first, "first")
  _write_trace(second, "second")

  started = threading.Event()
  release = threading.Event()
  calls: list[int] = []

  def blocking_merge(paths: list[Path], out_path: Path, slim: bool) -> None:
    calls.append(1)
    real_merge_traces(paths, out_path, slim)
    started.set()
    release.wait(10.0)

  monkeypatch.setattr(pages, "merge_traces", blocking_merge)

  async def run() -> None:
    paths = [first, second]
    leader = asyncio.create_task(pages._cached_merge(paths, False))
    assert await _wait_until(started.is_set)
    followers = [asyncio.create_task(pages._cached_merge(paths, False)) for _ in range(4)]
    release.set()
    results = await asyncio.gather(leader, *followers)
    assert len(calls) == 1
    assert len(list(pages._perfetto_merge_cache_dir().glob("*.json.gz"))) == 1
    bodies = [path.read_bytes() for path in results]
    assert all(body == bodies[0] for body in bodies)

  asyncio.run(run())


def test_single_flight_progress_independently(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cache_dir = tmp_path / "cache"
  monkeypatch.setattr(pages, "_perfetto_merge_cache_dir", lambda: cache_dir)
  monkeypatch.setattr(pages, "_merge_executor", lambda: None)

  key_a = [tmp_path / "a0.json", tmp_path / "a1.json"]
  key_b = [tmp_path / "b0.json", tmp_path / "b1.json"]
  for trace in (*key_a, *key_b):
    _write_trace(trace)

  calls: list[int] = []

  def counting_merge(paths: list[Path], out_path: Path, slim: bool) -> None:
    calls.append(1)
    real_merge_traces(paths, out_path, slim)

  monkeypatch.setattr(pages, "merge_traces", counting_merge)

  async def run() -> None:
    result_a, result_b = await asyncio.gather(
        pages._cached_merge(key_a, False), pages._cached_merge(key_b, False))
    assert result_a.is_file() and result_b.is_file()
    assert result_a != result_b
    assert len(calls) == 2
    assert len(list(pages._perfetto_merge_cache_dir().glob("*.json.gz"))) == 2

  asyncio.run(run())


def test_disconnect_does_not_lose_work(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cache_dir = tmp_path / "cache"
  monkeypatch.setattr(pages, "_perfetto_merge_cache_dir", lambda: cache_dir)
  monkeypatch.setattr(pages, "_merge_executor", lambda: None)
  key = [tmp_path / "rank0.json", tmp_path / "rank1.json"]
  _write_trace(key[0], "first")
  _write_trace(key[1], "second")

  started = threading.Event()
  release = threading.Event()
  calls: list[int] = []

  def blocking_merge(paths: list[Path], out_path: Path, slim: bool) -> None:
    calls.append(1)
    real_merge_traces(paths, out_path, slim)
    started.set()
    release.wait(10.0)

  monkeypatch.setattr(pages, "merge_traces", blocking_merge)

  async def run() -> None:
    waiter = asyncio.create_task(pages._cached_merge(key, False))
    assert await _wait_until(started.is_set)
    waiter.cancel()
    with pytest.raises(asyncio.CancelledError):
      await waiter
    # The shared build keeps running even though the waiter disconnected.
    release.set()
    assert await _wait_until(lambda: len(list(cache_dir.glob("*.json.gz"))) == 1)
    # A following request for the same key hits the now-cached entry, no second build.
    second_calls = len(calls)
    hit = await pages._cached_merge(key, False)
    assert hit.is_file()
    assert len(calls) == second_calls

  asyncio.run(run())
