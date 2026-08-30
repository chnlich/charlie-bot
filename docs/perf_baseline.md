# Perf baseline

Latency-perf baseline for the hourly cron in `prompts/cron/latency_perf.md`: metric definitions
with healthy ranges, seed measurements from the landing day, and the sampling history. Each round
compares its measurements against the healthy ranges here and lands at most one fix. This file
changes only through pull requests: a fix PR appends its sampling-history row, a PR that
introduces a metric with no row here adds that metric's definition row and history row in the same
PR, and a calibration-only round may open a docs-only PR of under 50 lines.

## Metric definitions

| Metric | Source | Unit | Healthy range (provisional) | Seed (2026-08-30) |
| --- | --- | --- | --- | --- |
| M1 host: load + serve CPU | `uptime`; M1 collector below | load 1/5/15; serve count; %CPU total | load < 4 (CPU count); serve CPU total < 300 % | 3.27 / 2.42 / 2.94; 4 serve processes, 237.9 % CPU |
| M2 UI polls | M2 collector below | polls/h; log MB | < 6000 polls/h | 2755 polls/h; 3.1 MB log |
| M3 API latency, 401 path | M3 collector below | seconds per request | median < 0.05 s | median 0.002 s, max 0.002 s |
| M4 turns | M4 collector below | seconds per turn; hung sessions | median < 300 s; hung = 0 | median 53 s, max 1133 s; 0 hung |
| M5 threads/list latency | M5 collector below | seconds per request, worst session | median < 0.05 s | — (introduced with its first history row) |

Note — every healthy range is provisional: a single-sample calibration from the 2026-08-30 seed
measurements against the design intent (load below the CPU count, serve CPU total well under
machine capacity, API median in the low tens of milliseconds, zero hung sessions). The serve CPU
ceiling sits at 75 % of the 4-CPU capacity because every round's reading includes its own worker
process, a constant bias. The M2 seed depends on how many browser tabs hold the dashboard open.
Any PR's Evidence section may recalibrate a range; the new value lands with that PR.

## Collector commands

The exact commands behind the seed values, run verbatim by the cron; `prompts/cron/latency_perf.md`
Step 1 carries the same commands, and the two copies stay identical.

M1 — host load and serve CPU:

```bash
uptime
ps -eo pcpu,args | grep '[o]pencode serve' | awk '{n++; s+=$1} END {printf "%d serve processes, %.1f%% cpu total\n", n, s}'
```

M2 — UI poll rate and server-log size, from the newest server log (its filename carries the server
start time; polls per hour is the status-request count divided by hours since that start):

```bash
LOG=$(ls -1t /tmp/charliebot-logs/server_*.log | head -1); python3 -c '
import os, re, sys, time
log = sys.argv[1]
start = time.mktime(time.strptime(re.search(r"server_(\d{8}_\d{6})", log).group(1), "%Y%m%d_%H%M%S"))
hours = (time.time() - start) / 3600
with open(log, errors="replace") as f:
    n = sum(1 for line in f if "sessions/status" in line or "tui/status" in line)
print(f"{n} status requests over {hours:.2f} h = {n / hours:.0f} polls/h; {os.path.getsize(log) / 1e6:.1f} MB")
' "$LOG"
```

M3 — API latency: the status check, then five credential-free timed requests (the 401 path — no
auth header; the timing reads the middleware-and-framework floor of the server path):

```bash
curl -s -o /dev/null -w 'http_code=%{http_code} time_total=%{time_total}s\n' http://127.0.0.1:18498/api/sessions/status
for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' http://127.0.0.1:18498/api/sessions/status; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M4 — turn durations and hung sessions. The projection reads only `type` and `timestamp` from chat
events and `status` from thread metadata; session content is never read:

```bash
python3 - <<'EOF'
import json
import statistics
from datetime import datetime, timedelta, timezone
from pathlib import Path

def event_ts(raw):
    if not raw:
        return None
    ts = datetime.fromisoformat(raw)
    return ts if ts.tzinfo else ts.replace(tzinfo=timezone.utc)

now = datetime.now(timezone.utc)
day_ago = now - timedelta(hours=24)
hour_ago = now - timedelta(hours=1)
root = Path.home() / ".charliebot" / "sessions"
durations = []
hung = 0
malformed = 0
for session_dir in root.iterdir():
    running = False
    threads_dir = session_dir / "threads"
    if threads_dir.is_dir():
        for thread_meta in threads_dir.glob("*/metadata.json"):
            if json.loads(thread_meta.read_text()).get("status") == "running":
                running = True
    events_path = session_dir / "data" / "chat_events.jsonl"
    if not events_path.is_file():
        continue
    recent = events_path.stat().st_mtime >= day_ago.timestamp()
    if not (running or recent):
        continue
    turn_start = None
    last_event = None
    with events_path.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                malformed += 1
                continue
            ts = event_ts(event.get("timestamp"))
            if ts is None:
                continue
            last_event = ts
            kind = event.get("type")
            if kind == "user":
                if turn_start is None:
                    turn_start = ts
            elif kind == "master_done" and turn_start is not None:
                if turn_start >= day_ago:
                    durations.append((ts - turn_start).total_seconds())
                turn_start = None
    if running and (last_event is None or last_event < hour_ago):
        hung += 1

median = statistics.median(durations) if durations else 0.0
peak = max(durations) if durations else 0.0
line = (f"{len(durations)} user->master_done turns in last 24h: "
        f"median {median:.0f}s, max {peak:.0f}s; {hung} running sessions with last event older than 1h")
if malformed:
    line += f"; {malformed} malformed event lines"
print(line)
EOF
```

M5 — threads/list latency for the session with the most thread metadata files on disk (the worst
case the 3 s workers-panel poll can hit; the key is read read-only from the host config):

```bash
KEY=$(grep -m1 '^charliebot_access_key:' ~/.charliebot/config.yaml | awk '{print $2}'); read SID N <<<"$(python3 -c '
from pathlib import Path
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    t = d / "threads"
    if t.is_dir():
        n = sum(1 for p in t.iterdir() if (p / "metadata.json").is_file())
        if n > best_n:
            best, best_n = d, n
print(best.name, best_n)
')"; echo "session $SID, $N threads"; for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' -H "Authorization: Bearer $KEY" "http://127.0.0.1:18498/api/threads/$SID/list"; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

## Sampling history

| Date | PR | Before → after | Note |
| --- | --- | --- | --- |
| 2026-08-30 | #457 | M5 median 0.068 s → 0.029 s (117-thread worst session) | one executor hop for the threads metadata scan; M5 definition and healthy range introduced with this PR |
