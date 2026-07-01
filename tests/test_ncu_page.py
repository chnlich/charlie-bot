from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import pages
from src.core.ncu_parsing import _extract_rules, parse_ncu_report

_SAMPLE_REPORT = Path("/data/home/chaoli/scripts/20260528_rmsnorm_ncu/out/ncu_cuda.ncu-rep")
_FULL_REPORT = Path("/data/home/chaoli/scripts/20260528_rmsnorm_ncu_s1856/out/ncu_cuda_full.ncu-rep")
_ROOFLINE_PLACEHOLDER = "本报告未采集 roofline 数据 (需要 --set full/detailed)"

_requires_sample = pytest.mark.skipif(not _SAMPLE_REPORT.is_file(), reason="sample ncu-rep not present on this host")
_requires_full_report = pytest.mark.skipif(not _FULL_REPORT.is_file(), reason="full ncu-rep not present on this host")


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
  assert _ROOFLINE_PLACEHOLDER in body
  assert '"available": false' in body


@_requires_sample
def test_ncu_renders_all_tabs_and_provenance(client: TestClient) -> None:
  body = client.get("/ncu", params={"file": str(_SAMPLE_REPORT)}).text
  for tab in ("summary", "details", "roofline", "source", "session", "raw"):
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
  assert report["roofline"]["available"] is False
  assert report["roofline"]["message"] == _ROOFLINE_PLACEHOLDER

  # Session / Device tab data is derived from device__attribute_* metrics.
  labels = {item["label"]: item["value"] for item in report["device_summary"]}
  assert labels["Device"] == "NVIDIA B200"
  assert labels["Compute capability"] == "10.0"
  assert len(report["device_attributes"]) > 100


@_requires_full_report
def test_parser_surfaces_roofline_for_full_report() -> None:
  report = parse_ncu_report(str(_FULL_REPORT))
  assert report["parser"] == "ncu_report"
  assert report["roofline"]["available"] is True
  assert len(report["kernels"]) == 2
  assert len(report["kernels"][0]["metrics"]) > 2500
  assert len(report["kernels"][0]["rules"]) == 18

  rooflines = {r["id"]: r for r in report["roofline"]["rooflines"]}
  assert set(rooflines) == {"fp32", "fp64"}
  fp32 = rooflines["fp32"]
  assert len(fp32["points"]) == 2
  point = fp32["points"][0]
  assert point["achieved_flops"] == pytest.approx(3.3188686360779385e12, rel=1e-6)
  assert point["arithmetic_intensity"] == pytest.approx(1.6763715359204332, rel=1e-6)
  assert point["compute_ceiling_flops"] == pytest.approx(4.355738696836371e13, rel=1e-6)
  assert point["memory_ceiling_bytes_s"] == pytest.approx(7.668032683846637e12, rel=1e-6)
  # NCU's SpeedOfLight_Roofline rule text rounds this to 8% of FP32 peak.
  assert round(100 * point["achieved_flops"] / point["compute_ceiling_flops"]) == 8


@_requires_full_report
def test_ncu_renders_full_report_roofline_tab(client: TestClient) -> None:
  resp = client.get("/ncu", params={"file": str(_FULL_REPORT)})
  assert resp.status_code == 200
  body = resp.text
  assert "2 kernels" in body
  assert 'data-tab="roofline"' in body
  assert "roofline-chart-shell" in body
  assert "SpeedOfLight_RooflineChart" in body
  assert '"available": true' in body


def test_extract_rules_reads_swig_attribute_objects() -> None:
  """rule_message()/speedup_estimation() return attribute objects, not dicts.

  The narrow sample report carries zero rules, so this guards the --set path:
  reading the message/speedup as attributes (not `.get(...)`) must yield a
  populated, colour-labelled rule rather than silently dropping it.
  """

  class _Msg:
    title = "Long Scoreboard Stalls"
    message = "On average each warp spends cycles stalled."
    type = 3  # warning

  class _Speedup:
    speedup = 12.5
    type = 1

  class _Rule:

    def name(self) -> str:
      return "IssueEfficiency"

    def section_identifier(self) -> str:
      return "WarpStateStats"

    def has_rule_message(self) -> bool:
      return True

    def rule_message(self) -> _Msg:
      return _Msg()

    def has_speedup_estimation(self) -> bool:
      return True

    def speedup_estimation(self) -> _Speedup:
      return _Speedup()

  class _Action:

    def rule_results(self) -> list[_Rule]:
      return [_Rule()]

  rules = _extract_rules(_Action())
  assert len(rules) == 1
  assert rules[0]["title"] == "Long Scoreboard Stalls"
  assert rules[0]["type_label"] == "warning"
  assert rules[0]["speedup_pct"] == 12.5
  assert rules[0]["section"] == "WarpStateStats"


def test_extract_rules_reads_dict_objects() -> None:
  """Newer ncu_report builds return dicts for message/speedup payloads."""

  class _Rule:

    def name(self) -> str:
      return "Issue Slot Utilization"

    def section_identifier(self) -> str:
      return "SchedulerStats"

    def has_rule_message(self) -> bool:
      return True

    def rule_message(self) -> dict:
      return {"title": "Latency Issue", "message": "Eligible warps are low.", "type": 2}

    def has_speedup_estimation(self) -> bool:
      return True

    def speedup_estimation(self) -> dict:
      return {"type": 1, "speedup": 63.8354}

  class _Action:

    def rule_results(self) -> list[_Rule]:
      return [_Rule()]

  rules = _extract_rules(_Action())
  assert len(rules) == 1
  assert rules[0]["title"] == "Latency Issue"
  assert rules[0]["type_label"] == "optimization"
  assert rules[0]["speedup_pct"] == 63.8354
