"""Emit a digest of user messages across all sessions for the memory curator's mining step (Step 2).

One line per user message from the last 7 days: `<YYYY-MM-DD> <session-short-id> [NEW] <text>`,
NEW marking messages from the last 24 hours. Read-only; output goes to stdout, capped at 120K
characters with the oldest lines dropped first.
"""
import datetime
import glob
import json
import os
import time

WINDOW_S = 7 * 86400
NEW_S = 86400
TEXT_CAP = 400
TOTAL_CAP = 120_000

now = time.time()
rows = []
for path in glob.glob(os.path.expanduser('~/.charliebot/sessions/*/data/chat_events.jsonl')):
  sid = path.split('/sessions/')[1][:8]
  if now - os.path.getmtime(path) > WINDOW_S:
    continue
  for line in open(path):
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if (event.get('type') or event.get('event_type')) != 'user':
      continue
    content = event.get('content')
    if not isinstance(content, str) or not content.strip() or content.startswith('/'):
      continue
    ts = (event.get('timestamp') or '').replace('Z', '+00:00')
    try:
      t = datetime.datetime.fromisoformat(ts).timestamp()
    except ValueError:
      continue
    if now - t > WINDOW_S:
      continue
    rows.append((t, sid, content[:TEXT_CAP].replace('\n', ' ')))
rows.sort()
out = [
    f"{datetime.datetime.fromtimestamp(t):%Y-%m-%d} {sid} {'NEW ' if now - t <= NEW_S else ''}{text}"
    for t, sid, text in rows
]
while sum(len(x) + 1 for x in out) > TOTAL_CAP:
  out.pop(0)
print('\n'.join(out))
