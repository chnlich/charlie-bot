from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import pages
from src.api.ncu_parsing import parse_ncu_report

_SAMPLE_REPORT = Path("/data/home/chaoli/scripts/20260528_rmsnorm_ncu/out/ncu_cuda.ncu-rep")

_requires_sample = pytest.mark.skipif(not _SAMPLE_REPORT.is_file(), reason="sample ncu-rep not present on this host")


@pytest.fixture
def client() -> TestClient:
  app = FastAPI()
  app.include_router(pages.router)
  return TestClient(app)


def test_ncu_missing_file_param_returns_clean_400(client: TestClient) -> None:
  resp = client.get("/ncu")
  assert resp.status_code == 400
  assert 'id="error"' in resp.text
  assert "No report specified" in resp.text


def test_ncu_relative_path_returns_clean_400(client: TestClient) -> None:
  resp = client.get("/ncu", params={"file": "relative/path.ncu-rep"})
  assert resp.status_code == 400
  assert "must be absolute" in resp.text


def test_ncu_missing_report_returns_clean_404(client: TestClient) -> None:
  resp = client.get("/ncu", params={"file": "/tmp/does-not-exist.ncu-rep"})
  assert resp.status_code == 404
  assert "Report not found" in resp.text
  # Never leak a 500 stack trace.
  assert "Traceback" not in resp.text


def test_ncu_junk_file_returns_clean_4xx(client: TestClient, tmp_path: Path) -> None:
  junk = tmp_path / "not-a-report.ncu-rep"
  junk.write_bytes(b"this is not a valid ncu report\x00\x01\x02" * 50)
  resp = client.get("/ncu", params={"file": str(junk)})
  assert 400 <= resp.status_code < 500
  assert 'id="error"' in resp.text
  assert "Traceback" not in resp.text


@_requires_sample
def test_ncu_renders_sample_report(client: TestClient) -> None:
  resp = client.get("/ncu", params={"file": str(_SAMPLE_REPORT)})
  assert resp.status_code == 200
  body = resp.text
  # 6 kernels, embedded report data, and the handoff block.
  assert "6 kernels" in body
  assert "const REPORT =" in body
  assert f"/files{_SAMPLE_REPORT}" in body
  assert f"ncu-ui {_SAMPLE_REPORT}" in body
  assert f"ncu --import {_SAMPLE_REPORT} --page details" in body


@_requires_sample
def test_ncu_renders_all_tabs_and_provenance(client: TestClient) -> None:
  body = client.get("/ncu", params={"file": str(_SAMPLE_REPORT)}).text
  for tab in ("summary", "details", "source", "session", "raw"):
    assert f'data-tab="{tab}"' in body
  # The Raw tab is labelled the complete source of truth; other tabs are labelled
  # as rendered views or additional content.
  assert "Complete source of truth" in body
  assert "Rendered view" in body
  assert "Additional content" in body


@_requires_sample
def test_parser_surfaces_new_content_types() -> None:
  report = parse_ncu_report(str(_SAMPLE_REPORT))
  assert report["parser"] == "ncu_report"
  assert report["device"] == "NVIDIA B200"
  assert len(report["kernels"]) == 6

  kernel = report["kernels"][0]
  # Raw set is the complete ~281-metric source of truth.
  assert len(kernel["metrics"]) > 250
  # Narrow --metrics report: a single section, no rule recommendations.
  assert [s["name"] for s in kernel["sections"]] == ["Command line profiler metrics"]
  assert kernel["rules"] == []
  # SASS is reconstructed via the module; PTX and CUDA-C were not collected.
  assert len(kernel["sass"]) > 100
  assert kernel["sass"][0]["addr"].startswith("0x")
  assert kernel["ptx"] == []
  assert kernel["cuda_sources"] == []

  # Session / Device tab data is derived from device__attribute_* metrics.
  labels = {item["label"]: item["value"] for item in report["device_summary"]}
  assert labels["Device"] == "NVIDIA B200"
  assert labels["Compute capability"] == "10.0"
  assert len(report["device_attributes"]) > 100
