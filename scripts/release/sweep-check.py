"""The startup sweep-name check, shared by switch-rehearsal.sh and trial.sh.

sweep-check.py LOG SESSIONS

LOG is one server log in console or JSON-lines form. Its session_cgroup_sweep_removed lines name the cgroups that the
server's startup sweep removed. SESSIONS is that server's sessions directory. A removed name is foreign when no session
directory in SESSIONS names it: a session's cgroup name is charliebot-sess- and the first 8 characters of its directory
name. Prints the line count, the removed names and the foreign ones; exits 1 when a removed name is foreign. A missing
LOG raises.
"""
import ast
import json
import pathlib
import re
import sys

CONSOLE = re.compile(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d{3})? \[\w+\s*\] (\S+)\s*(.*)")
FIELD = re.compile(r"(\w+)=(\[[^\]]*\]|\S+)")
EVENT = "session_cgroup_sweep_removed"
PREFIX = "charliebot-sess-"


def removed_names(line):
    """The names a session_cgroup_sweep_removed line removed, or None for any other line."""
    text = line.strip()
    if text.startswith("{"):
        row = json.loads(text)
        if row.get("event") != EVENT:
            return None
        # The Rust server logs `names = ?removed`, which its JSON formatter writes as the Debug text of the list:
        # a string holding a JSON array.
        names = row["names"]
        return json.loads(names) if isinstance(names, str) else names
    match = CONSOLE.match(text)
    if match is None or match[1] != EVENT:
        return None
    return ast.literal_eval(dict(FIELD.findall(match[2]))["names"])


def session_cgroup_names(sessions):
    root = pathlib.Path(sessions)
    if not root.is_dir():
        return set()
    return {PREFIX + entry.name[:8] for entry in root.iterdir() if entry.is_dir()}


def main(log_path, sessions):
    own = session_cgroup_names(sessions)
    text = pathlib.Path(log_path).read_text(encoding="utf-8", errors="replace")
    lines = 0
    removed = []
    # Complete lines only: a line the server is still writing has no newline yet.
    for line in text.split("\n")[:-1]:
        names = removed_names(line)
        if names is not None:
            lines += 1
            removed.extend(names)
    foreign = sorted(name for name in removed if name not in own)
    print(f"sweep_removed_lines={lines} removed={','.join(sorted(removed)) or 'none'} foreign={','.join(foreign) or 'none'}")
    return 1 if foreign else 0


if __name__ == "__main__":
    raise SystemExit(main(*sys.argv[1:]))
