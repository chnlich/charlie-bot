"""Write the Nsight reports the features-ncu scenario reads: a two-kernel report with rules, SASS, PTX, CUDA source and
a roofline point, a report the module refuses, and a report with no kernels.

Each report is the JSON the fake install reads (rust/parity/fakes/nsight): its kernels feed the fake `ncu_report`
module, and its `csv` text is what the fake `ncu` prints for `--page details`.
"""

import csv
import io
import json

from rust.parity.fixture import Builder

# The SpeedOfLight_RooflineChart metrics of an FP32 kernel, so the page draws one roofline point.
ROOFLINE_FP32 = [
    ("derived__sm__sass_thread_inst_executed_op_ffma_pred_on_x2", 512, ""),
    ("sm__cycles_elapsed.avg.per_second", 1.41e9, ""),
    ("smsp__cycles_elapsed.avg.per_second", 1.35e9, ""),
    ("dram__bytes.sum.peak_sustained", 320, ""),
    ("dram__cycles_elapsed.avg.per_second", 6.0e9, ""),
    ("dram__bytes.sum.per_second", 2.5e11, ""),
    ("smsp__sass_thread_inst_executed_op_fadd_pred_on.sum.per_cycle_elapsed", 4.0, ""),
    ("smsp__sass_thread_inst_executed_op_fmul_pred_on.sum.per_cycle_elapsed", 4.0, ""),
    ("derived__smsp__sass_thread_inst_executed_op_ffma_pred_on_x2", 8.0, ""),
    ("derived__smsp__sass_thread_inst_executed_op_fadd2_pred_on_x2", 2.0, ""),
]

CSV_HEADER = [
    "ID", "Kernel Name", "Block Size", "Grid Size", "CC", "Section Name", "Metric Name", "Metric Unit", "Metric Value"
]


def _metric(name: str, value: float, unit: str) -> dict:
  return {"name": name, "value": value, "unit": unit}


def _kernel_csv(idx: int, name: str, block: str, grid: str, sections: list[tuple[str, str, str, float]]) -> list[list]:
  return [
      [str(idx), name, block, grid, "8.6", section, metric, unit, value] for section, metric, unit, value in sections
  ]


def _two_kernel_report() -> dict:
  gemm_metrics = [
      _metric("gpu__time_duration.sum", 1234.5, "usecond"),
      _metric("sm__throughput.avg.pct_of_peak_sustained_elapsed", 72.25, "%"),
      _metric("gpu__compute_memory_throughput.avg.pct_of_peak_sustained_elapsed", 55.5, "%"),
      _metric("sm__warps_active.avg.pct_of_peak_sustained_active", 41.25, "%"),
      _metric("launch__registers_per_thread", 32, "register/thread"),
  ] + [_metric(name, value, unit) for name, value, unit in ROOFLINE_FP32]
  copy_metrics = [
      _metric("gpu__time_duration.sum", 7.0, "msecond"),
      _metric("sm__warps_active.avg.pct_of_peak_sustained_active", 88.0, "%"),
  ]
  kernels = [
      {
          "name": "synthetic_gemm",
          "metrics": gemm_metrics,
          "function_pcs": [0],
          "rules":
              [
                  {
                      "name": "SOLBottleneck",
                      "section": "GPU Speed Of Light Throughput",
                      "title": "Compute Bottleneck",
                      "message": "Synthetic throughput advice.",
                      "type": 1,
                  },
                  {
                      "name": "CPIStall",
                      "section": "WarpStateStats",
                      "title": "Warp Stall",
                      "message": "Synthetic stall advice.",
                      "type": 2,
                      "speedup_pct": 12.5,
                  },
              ],
          "sass":
              [
                  {
                      "pc": 0,
                      "text": "MOV R1, c[0x0][0x28] ;"
                  },
                  {
                      "pc": 16,
                      "text": "FFMA R0, R2, R3, R0 ;"
                  },
                  {
                      "pc": 32,
                      "text": "STG.E [R4], R0 ;"
                  },
                  {
                      "pc": 48,
                      "text": "EXIT ;"
                  },
              ],
          "ptx":
              [
                  {
                      "pc": 16,
                      "text": "fma.rn.f32 %f3, %f1, %f2, %f3;"
                  },
                  {
                      "pc": 32,
                      "text": "st.global.f32 [%rd1], %f3;"
                  },
              ],
          "source_files": {
              "synthetic_gemm.cu": "__global__ void synthetic_gemm(float *c) { c[threadIdx.x] = 0.0f; }\n"
          },
      },
      {
          "name": "synthetic_copy",
          "metrics": copy_metrics,
          "rules": [],
          "sass": [],
          "ptx": [],
          "source_files": {},
      },
  ]
  rows = [CSV_HEADER]
  rows += _kernel_csv(
      0, "synthetic_gemm", "(128, 1, 1)", "(64, 6, 1)", [
          ("GPU Speed Of Light Throughput", "gpu__time_duration.sum", "usecond", 1234.5),
          ("GPU Speed Of Light Throughput", "sm__throughput.avg.pct_of_peak_sustained_elapsed", "%", 72.25),
          ("Occupancy", "sm__warps_active.avg.pct_of_peak_sustained_active", "%", 41.25),
          ("Launch Statistics", "launch__registers_per_thread", "register/thread", 32),
      ])
  rows += _kernel_csv(
      1, "synthetic_copy", "(256, 1, 1)", "(1024, 1, 1)", [
          ("GPU Speed Of Light Throughput", "gpu__time_duration.sum", "msecond", 7.0),
          ("Occupancy", "sm__warps_active.avg.pct_of_peak_sustained_active", "%", 88.0),
      ])
  buffer = io.StringIO()
  csv.writer(buffer, lineterminator="\n").writerows(rows)
  return {"kernels": kernels, "csv": buffer.getvalue()}


async def build(b: Builder) -> dict:
  reports = {
      "parsed": _two_kernel_report(),
      "refused": {
          "refuse": "synthetic refusal",
          "kernels": [],
          "csv": ""
      },
      "empty": {
          "kernels": [],
          "csv": ""
      },
  }
  root = b.cfg.charliebot_home.parent / "workspace" / "ncu"
  root.mkdir(parents=True)
  for name, report in reports.items():
    (root / f"{name}.ncu-rep").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
  # Tree-relative, as every fixture extra's values must be; `missing` is named but never written.
  return {name: f"workspace/ncu/{name}.ncu-rep" for name in ("parsed", "refused", "empty", "missing")}
