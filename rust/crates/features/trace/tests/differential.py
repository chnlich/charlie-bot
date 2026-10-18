"""Build Python reference artifacts and compare them with the Rust merge builders."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile
from collections.abc import Callable
from typing import Any

ROOT = pathlib.Path(__file__).resolve().parents[5]
RUST_ROOT = ROOT / "rust"
CARGO_SLOT = pathlib.Path("/home/user/scripts/20261018_rustport/cargo-slot.sh")


def make_cases(directory: pathlib.Path) -> list[dict[str, Any]]:
  corpus = directory / "corpus"
  corpus.mkdir()
  cases: list[dict[str, Any]] = []

  def add(name: str, files: dict[str, bytes], paths: list[str], slim: bool = False) -> None:
    case_dir = corpus / name
    case_dir.mkdir()
    for filename, content in files.items():
      (case_dir / filename).write_bytes(content)
    cases.append({"name": name, "paths": [str(case_dir / path) for path in paths], "slim": slim})

  add("bare-array", {"events.json": b'[{"ph":"X","pid":1,"tid":2,"ts":1,"dur":3}]'}, ["events.json"])
  add(
      "metadata-siblings-and-numbers",
      {
          "rank12.json":
              (
                  '{"deviceProperties":{"vendor":"Météo 🧪","large":18446744073709551617},'
                  '"traceEvents":[{"ph":"X","pid":-9223372036854775808,"tid":18446744073709551617,'
                  '"ts":-1.25,"dur":3.5,"name":"float and non-ASCII 🧪"}]}'.encode())
      },
      ["rank12.json"],
  )
  add(
      "missing-and-string-tids",
      {
          "rank4.json":
              (
                  b'{"traceEvents":[{"ph":"X","pid":1,"name":"missing tid"},'
                  b'{"ph":"X","pid":1,"tid":"17","name":"string tid"},'
                  b'{"ph":"X","pid":1,"tid":17,"name":"number tid"}]}')
      },
      ["rank4.json"],
  )
  add(
      "flow-id-phases",
      {
          "flow.json":
              (
                  b'{"traceEvents":[{"ph":"s","pid":-7,"tid":"thread","id":"flow-a"},'
                  b'{"ph":"t","pid":-7,"tid":"thread","id":"flow-a"},'
                  b'{"ph":"f","pid":-7,"tid":"thread","id":"flow-b"}]}')
      },
      ["flow.json"],
  )
  add(
      "process-labels-and-slim",
      {
          "rank2.json":
              (
                  b'{"traceEvents":[{"ph":"M","name":"process_labels","pid":7,"args":{"labels":"GPU 2"}},'
                  b'{"ph":"M","name":"process_labels","pid":8,"args":{"labels":"CPU"}},'
                  b'{"ph":"M","name":"process_name","pid":9,"name":"process_meta"},'
                  b'{"ph":"X","pid":7,"tid":2,"cat":"cpu_instant_event","name":"drop"},'
                  b'{"ph":"X","pid":7,"tid":2,"args":{"stream":"copy","other":1},"name":"keep stream"},'
                  b'{"ph":"X","pid":8,"tid":3,"args":{"other":2},"name":"remove args"}]}')
      },
      ["rank2.json"],
      slim=True,
  )
  add(
      "multi-member-skip",
      {
          "rank0.json": b'[{"ph":"X","pid":1,"tid":1,"name":"valid"}]',
          "analysis.json": b'{"analysis":[]}',
          "rank1.json": b'{"traceEvents":[{"ph":"X","pid":2,"tid":2}]}'
      },
      ["rank0.json", "analysis.json", "rank1.json"],
  )
  add("empty-file", {"empty.json": b""}, ["empty.json"])
  add("all-members-non-trace", {"analysis.json": b'{"analysis":[]}'}, ["analysis.json"])
  add("non-object-event", {"events.json": b"[1]"}, ["events.json"])
  add(
      "truthy-non-object-label-args",
      {"events.json": b'{"traceEvents":[{"ph":"M","name":"process_labels","args":true}]}'},
      ["events.json"],
  )
  add(
      "truthy-non-string-metadata-name",
      {"events.json": b'{"traceEvents":[{"ph":"M","name":4}]}'},
      ["events.json"],
  )

  malformed = {
      "unexpected-end": b"{",
      "trailing-content": b"{}x",
      "missing-colon": b'{"a" 1}',
      "missing-comma": b'{"a":1 "b":2}',
      "invalid-value": b'{"a":?}',
      "invalid-escape": b'"\\x"',
      "unterminated-string": b'"abc',
      "leading-zero": b"01",
      "invalid-object-key": b"{1:2}",
      "missing-array-comma": b"[1 2]",
      "missing-object-value": b'{"a":}',
      "nan-literal": b"NaN",
      "infinity-literal": b"Infinity",
      "negative-infinity-literal": b"-Infinity",
      "control-character": b'"a\nb"',
      "overflow-float": b"1e9999",
      "invalid-unicode-escape": b'"\\u12xz"',
  }
  for name, content in malformed.items():
    add(name, {"malformed.json": content}, ["malformed.json"])

  events = [
      {
          "ph": "X",
          "pid": index % 3,
          "tid": index % 17,
          "ts": index / 3,
          "dur": -index,
          "name": f"event-{index}"
      } for index in range(513)
  ]
  add(
      "batch-boundary-513",
      {"rank513.json": json.dumps({
          "traceEvents": events
      }, separators=(",", ":"), ensure_ascii=False).encode()},
      ["rank513.json"],
  )
  return cases


def capture_reference(function: Callable[[], None], output: pathlib.Path) -> dict[str, Any]:
  output.unlink(missing_ok=True)
  try:
    function()
  except Exception as error:  # The reference builders expose these exceptions to the route.
    return {"error": str(error)}
  return {"gzip": str(output)}


def write_references(directory: pathlib.Path, cases: list[dict[str, Any]]) -> None:
  sys.path.insert(0, str(ROOT))
  from src.features.trace import api, trace_merge

  refs = directory / "references"
  refs.mkdir()
  for case in cases:
    paths = [pathlib.Path(path) for path in case["paths"]]
    case_refs = refs / case["name"]
    case_refs.mkdir()
    builds: dict[str, Callable[[], None]] = {
        "merge": lambda: trace_merge.merge_traces(paths, case_refs / "merge.json.gz", case["slim"]),
        "multi": lambda: trace_merge.build_multi_trace_merge(paths, case_refs / "multi.json.gz", case["slim"], None),
        "direct": lambda: api._build_direct_pass_gzip(paths[0], case_refs / "direct.json.gz"),
    }
    case["references"] = {
        name: capture_reference(build, case_refs / f"{name}.json.gz") for name, build in builds.items()
    }
    case["cache_keys"] = {
        "gzip": api._merge_cache_key(paths, case["slim"], "gzip"),
        "merge": api._merge_cache_key(paths, case["slim"], "merge"),
    }
  (directory / "manifest.json").write_text(json.dumps(cases, ensure_ascii=False), encoding="utf-8")


def main() -> int:
  with tempfile.TemporaryDirectory(prefix="trace-differential-") as temporary:
    directory = pathlib.Path(temporary)
    python_tmp = directory / "python-tmp"
    python_tmp.mkdir()
    tempfile.tempdir = str(python_tmp)
    os.environ["TMPDIR"] = str(python_tmp)
    cases = make_cases(directory)
    write_references(directory, cases)
    environment = os.environ.copy()
    environment["TRACE_DIFFERENTIAL_DIR"] = str(directory)
    result = subprocess.run(
        [
            str(CARGO_SLOT),
            "test",
            "-p",
            "charliebot-trace",
            "differential_tests::python_and_rust_trace_builders_match_seeded_corpus",
            "--",
            "--ignored",
            "--nocapture",
        ],
        cwd=RUST_ROOT,
        env=environment,
        check=False,
    )
    if result.returncode != 0:
      return result.returncode
    print(f"trace differential: {len(cases)} cases × 3 builders plus 2 cache-key modes; zero differences")
    return 0


if __name__ == "__main__":
  raise SystemExit(main())
