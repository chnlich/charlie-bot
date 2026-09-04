"""CLI: publish an artifact to the URL readers beyond the operator's devices open.

  charliebot publish <artifact-path>

Copies the file into the configured publish directory (mode 0644, same-name
overwrite) through the one publish action (src/core/publish.py) and prints the
published URL on stdout. A differing file replaced under the same name is reported
as a note naming the replaced file. A preflight failure — publish lane
unconfigured, artifact missing — prints a JSON error naming the missing item on
stderr and exits 1; nothing is published and no URL falls back to the server port.
"""

import argparse
import json
import sys

from src.core.config import get_config
from src.core.publish import PublishError, publish_artifact


def main() -> None:
  parser = argparse.ArgumentParser(description="Publish an artifact and print the URL readers outside use")
  parser.add_argument("artifact", help="Path of the artifact file to publish")
  args = parser.parse_args()

  try:
    result = publish_artifact(args.artifact, get_config())
  except PublishError as e:
    print(json.dumps({"error": str(e)}), file=sys.stderr)
    sys.exit(1)
  print(result.url)
  if result.overwrote:
    print(json.dumps({"note": f"overwrote a differing file with the same name: {result.path}"}), file=sys.stderr)


if __name__ == "__main__":
  main()
