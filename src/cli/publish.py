"""CLI: publish an artifact to the URL readers beyond the operator's devices open.

  charliebot publish <artifact-path>

Copies the file to ``<publish.dir>/<token>/<basename>`` — a fresh unguessable
directory per call, so nothing is overwritten — through the one publish action
(src/core/publish.py) and prints the published URL on stdout. A preflight
failure — publish lane unconfigured or its index.html missing, artifact missing
— prints a JSON error naming the missing item on stderr and exits 1; nothing is
published and no URL falls back to the server port.
"""

import argparse

from src.cli import common as cli_common
from src.cli import help_formatter


def main() -> None:
  parser = argparse.ArgumentParser(
      description="Publish an artifact and print the URL readers outside use",
      formatter_class=help_formatter.CliHelpFormatter)
  parser.add_argument("artifact", help="Path of the artifact file to publish")
  args = parser.parse_args()
  # The publish and config stacks ride the one publish that needs them: a
  # deferral here keeps --help and parser errors off their import chains (the
  # src.cli.config deferral shape).
  from src.core import config, publish

  try:
    result = publish.publish_artifact(args.artifact, config.get_config())
  except publish.PublishError as e:
    cli_common.exit_error(str(e))
  print(result.url)


if __name__ == "__main__":
  main()
