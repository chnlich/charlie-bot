from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import pages

_SAMPLE_REPORT = Path("/data/home/chaoli/scripts/20260528_rmsnorm_ncu/out/ncu_cuda.ncu-rep")


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


@pytest.mark.skipif(not _SAMPLE_REPORT.is_file(), reason="sample ncu-rep not present on this host")
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
