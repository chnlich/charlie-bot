"""The Python side of tests/file_links_cross_implementation.rs: `src/runtime/file_urls.py` over one cases file.

Usage: uv run python rust/crates/infra/compat/tests/file_links_python.py <cases.json> <answers.json>

Each case is {"text": str, "context": null or {server_port, charliebot_home, session_dir, user_home}}. The answers
file holds one answer per case, in order: {"findings": [...], "detail": str}. A finding names its link, form, path,
live and correct_link; detail is dead_links_detail of the findings.
"""

import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))

from src.runtime import file_urls  # noqa: E402


def scan_context(raw: dict | None) -> file_urls.ScanContext | None:
  if raw is None:
    return None
  return file_urls.ScanContext(
      server_port=raw["server_port"],
      charliebot_home=pathlib.Path(raw["charliebot_home"]),
      session_dir=pathlib.Path(raw["session_dir"]),
      user_home=pathlib.Path(raw["user_home"]),
  )


def answer(case: dict) -> dict:
  findings = file_urls.scan_file_links(case["text"], scan_context(case["context"]))
  return {
      "findings":
          [
              {
                  "link": finding.link,
                  "form": finding.form.value,
                  "path": None if finding.path is None else str(finding.path),
                  "live": finding.live,
                  "correct_link": finding.correct_link,
              } for finding in findings
          ],
      "detail": file_urls.dead_links_detail(findings),
  }


def main() -> int:
  cases = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
  answers = [answer(case) for case in cases]
  pathlib.Path(sys.argv[2]).write_text(json.dumps(answers, ensure_ascii=False, indent=1), encoding="utf-8")
  return 0


if __name__ == "__main__":
  sys.exit(main())
