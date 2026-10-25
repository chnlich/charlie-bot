"""CLI: publish an artifact to the URL readers beyond the operator's devices open.

  charliebot publish <artifact-path> [-- <lane option>...]

Publishes through the one publish action (src/features/artifacts/publish.py) and prints the
published URL on stdout. With ``publish.command`` set, the external program gets the artifact's
absolute path, then every argument after ``--`` unchanged. Without it, the file is copied to
``<publish.dir>/<token>/<basename>`` — a fresh unguessable directory per call, so nothing is
overwritten — and the directory lane takes no lane options. A failure — a missing config key, a
missing artifact, or a program that exits non-zero or does not print exactly one URL — prints a
JSON error on stderr and exits 1; no URL is printed and none falls back to the server port.
"""

import sys

from src.infra import help_formatter
from src.runtime.cli import common as cli_common


def main() -> None:
  parser = help_formatter.CliArgumentParser(
      description="Publish an artifact and print the URL readers outside use",
      epilog="With publish.command set, arguments after -- go unchanged to its program; the directory lane takes none.")
  parser.add_argument("artifact", help="Path of the artifact file to publish")
  # Everything after the first "--" is a lane option, passed through unparsed.
  argv = sys.argv[1:]
  separator = argv.index("--") if "--" in argv else len(argv)
  args = parser.parse_args(argv[:separator])
  lane_options = argv[separator + 1:]
  from src.features.artifacts import publish  # deferred: charliebot publish --help
  from src.infra import config  # deferred: charliebot publish --help

  try:
    result = publish.publish_artifact(args.artifact, config.get_config(), lane_options)
  except publish.PublishError as e:
    cli_common.exit_error(str(e))
  print(result.url)


if __name__ == "__main__":
  main()
