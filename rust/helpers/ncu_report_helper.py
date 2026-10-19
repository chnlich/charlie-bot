"""Parse one .ncu-rep report through NVIDIA's ncu_report module, for the Rust server's ncu viewer.

The Rust server runs this with <repo>/.venv/bin/python, the repo root as its working directory and on sys.path, and
the absolute report path as its only argument. It prints one JSON object:

  {"status": "ok", "report": <the dict _parse_with_module returns>}
  {"status": "module_unavailable", "python_dir": <the extras/python dir the import tried, or null>}
  {"status": "parse_error", "error": <the NcuParseError text>}

Any other exception ends the process with a nonzero status and a traceback on stderr.
"""

import json
import sys

from src.features.ncu import ncu_parsing


def main() -> None:
  # structlog's default logger writes to stdout, and stdout must hold only the reply. Logs go to stderr instead.
  reply_stream = sys.stdout
  sys.stdout = sys.stderr
  abspath = sys.argv[1]
  module = ncu_parsing._load_ncu_report_module()
  if module is None:
    reply = {"status": "module_unavailable", "python_dir": ncu_parsing._discover_ncu_python_dir()}
  else:
    try:
      reply = {"status": "ok", "report": ncu_parsing._parse_with_module(module, abspath)}
    except ncu_parsing.NcuParseError as exc:
      reply = {"status": "parse_error", "error": str(exc)}
  reply_stream.write(json.dumps(reply) + "\n")


if __name__ == "__main__":
  main()
