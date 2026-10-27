"""Read one Rust-written boot marker with the Python timestamp parser."""

import datetime
import json
import pathlib
import sys


marker = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
instant = datetime.datetime.fromisoformat(marker["pass_started_at"])
print(instant.astimezone(datetime.timezone.utc).isoformat())
