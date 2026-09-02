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
| M6 session usage latency | M6 collector below | seconds per request, worst session | median < 0.05 s | — (introduced with its first history row) |
| M7 token-usage page | M7 collector below | seconds per page load | median < 3 s | — (introduced with its first history row) |
| M8 sidebar search, absent needle | M8 collector below | seconds per request | median < 0.5 s | — (introduced with its first history row) |
| M9 ext-usage codex spend rescan, steady state | M9 collector below | seconds per poll round | median < 0.05 s | — (introduced with its first history row) |
| M10 thread-metadata torn reads | M10 collector below | torn reads per concurrent save stream | 0 torn reads | — (introduced with its first history row) |
| M11 backlog reads on this host | M11 collector below | HTTP status of GET /api/backlog + /api/backlog/history | both 200; 0 backlog 500s in the server log | — (introduced with its first history row) |
| M12 ext-usage codex usage scrape, steady state | M12 collector below | seconds per poll round | median < 0.05 s | — (introduced with its first history row) |
| M13 thread-events read+transform, steady state | M13 collector below | seconds per read, worst on-disk worker log | median < 0.05 s | — (introduced with its first history row) |
| M14 git diff API event-loop lag | M14 collector below | seconds of loop lag per diff/files run, charlie-bot root..HEAD | median < 0.05 s | — (introduced with its first history row) |
| M15 recap-summary cache torn reads | M15 collector below | torn reads per concurrent write stream | 0 torn reads | — (introduced with its first history row) |
| M16 trigger-file torn reads | M16 collector below | torn reads per concurrent save stream | 0 torn reads | — (introduced with its first history row) |
| M17 session fork (clone) latency | M17 collector below | seconds per fork of the heaviest real session, scratch home | median < 2 s | — (introduced with its first history row) |
| M18 hidden-tab periodic poll fetches | M18 collector below | poll fetches per simulated 10 hidden minutes | 0 fetches | — (introduced with its first history row) |
| M19 SSE framing, chunked large-frame stream | M19 collector below | seconds per 16 MB payload (16 KB chunks, ~1 MB frames) | median < 0.2 s | — (introduced with its first history row) |
| M20 recap extract, repeat divider | M20 collector below | seconds per extract at one divider, worst on-disk extract corpus | median < 0.05 s | — (introduced with its first history row) |
| M21 sidebar probe sweep, steady state | M21 collector below | seconds per 10th-poll sweep over all active sessions | median < 0.05 s | — (introduced with its first history row) |
| M22 ext-usage unknown-limit-shape warning stream, steady state | M22 collector below | warnings per 60 steady-state transform rounds | 0 warnings after the first sighting per process | — (introduced with its first history row) |
| M23 archive-range chat-event rescan, steady state | M23 collector below | seconds per 8-page backwards scroll over the biggest archived corpus | median < 0.005 s | — (introduced with its first history row) |
| M24 trigger list, steady state | M24 collector below | seconds per list_triggers call, worst on-disk trigger corpus | median < 0.05 s | — (introduced with its first history row) |
| M25 scheduler config reload, steady state | M25 collector below | seconds of loop lag per steady-state reload, live config corpus | median < 0.005 s | — (introduced with its first history row) |
| M26 message-projection advance per appended event | M26 collector below | seconds per `get_message_projection` advance on one appended event, worst on-disk live-events corpus | median < 0.005 s | — (introduced with its first history row) |
| M27 plans registry tolerant read, steady state | M27 collector below | seconds per `read_plans_tolerant` call, worst on-disk plans corpus | median < 0.005 s | — (introduced with its first history row) |
| M28 ndjson tail+count scan, steady state | M28 collector below | seconds per `parse_ndjson_tail` call, worst on-disk live chat file | median < 0.030 s | — (introduced with its first history row) |
| M29 session-metadata listing preamble, steady state | M29 collector below | seconds per `_load_session_metas(ACTIVE)` call, live session-dir corpus | median < 0.005 s | — (introduced with its first history row) |
| M30 live-half chat-event range rescan, steady state | M30 collector below | seconds per 8-page backwards scroll over the biggest archived session's live file | median < 0.005 s | — (introduced with its first history row) |
| M31 worker-finalize events-summary read, steady state | M31 collector below | seconds per `read_events_summary` call, worst on-disk worker log | median < 0.02 s | — (introduced with its first history row) |
| M32 memory-store assemble, steady state | M32 collector below | seconds per `assemble_master` call, live memory corpus | median < 0.005 s | — (introduced with its first history row) |
| M33 assistant-stream draft render, full-turn replay | M33 collector below | seconds per replay of the largest on-disk assistant draft, 200 B deltas at 40 ms virtual cadence | median < 1.0 s | — (introduced with its first history row) |

Note — every healthy range is provisional: a single-sample calibration from the 2026-08-30 seed
measurements against the design intent (load below the CPU count, serve CPU total well under
machine capacity, API median in the low tens of milliseconds, zero hung sessions). The serve CPU
ceiling sits at 75 % of the 4-CPU capacity because every round's reading includes its own worker
process, a constant bias. The M2 seed depends on how many browser tabs hold the dashboard open.
Any PR's Evidence section may recalibrate a range; the new value lands with that PR.

## Collector commands

The exact commands behind the seed values, run verbatim by the cron; the cron prompt does not
duplicate them — this file is their single home.

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

M6 — session usage latency for the session with the most lines in its live `chat_events.jsonl` (the
worst case the 3 s active-session-view poll can hit while that session is thinking; only the live
file feeds resolution, archived events do not):

```bash
KEY=$(grep -m1 '^charliebot_access_key:' ~/.charliebot/config.yaml | awk '{print $2}'); read SID N <<<"$(python3 -c '
from pathlib import Path
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
print(best.name, best_n)
')"; echo "session $SID, $N chat events"; for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' -H "Authorization: Bearer $KEY" "http://127.0.0.1:18498/api/sessions/$SID/usage"; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M7 — token-usage page load: five timed requests of the rendered page. The page builds a
persisted tally cache covering all three sources: per-file entries for the gigabyte-scale
Claude logs and the hundred-megabyte Codex rollouts, and one entry for the opencode db's
whole contribution signatured on the main file plus its `-wal` sidecar (a WAL-mode write
leaves the main file untouched, so the main file's stat alone can never see it). The first
load after a server start (or after bulk log churn) is a full scan, later loads re-scan
only the sources that changed, so the five-request median reads the warm steady state
(~2 s at the seed host's 2.2 GB + 190 MB log volume, measured before the Codex logs joined
the cache — hence the provisional 3 s ceiling). Evidence while the live server runs older
code is a scratch-instance
A/B: live-before against this instance, scratch-after against a scratch server on the changed
code with a scratch `CHARLIEBOT_HOME` (its own empty cache directory — cold-then-warm measures
both paths):

```bash
KEY=$(grep -m1 '^charliebot_access_key:' ~/.charliebot/config.yaml | awk '{print $2}'); for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' -H "Authorization: Bearer $KEY" http://127.0.0.1:18498/token-usage; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M8 — sidebar search latency, worst case: five timed requests for a needle absent from every
active session's live chat file, so each request scans the whole searchable corpus (active
sessions' `chat_events.jsonl`) without an early hit. Evidence while the live server runs older
code is a scratch-instance A/B on the search corpus (all session metadata plus active sessions'
live chat files), the same shape as the M7 protocol.

```bash
KEY=$(grep -m1 '^charliebot_access_key:' ~/.charliebot/config.yaml | awk '{print $2}'); for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' -H "Authorization: Bearer $KEY" "http://127.0.0.1:18498/api/sessions/search?q=zzq9xneverpresentneedle77"; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M9 — ext-usage poller codex spend rescan, steady state. The poller's cost is background work invisible to HTTP probes, so the collector times the function the poller calls every round: the provider's spend computation over the live corpus (read-only), from the main repo checkout. The collector reports the no-churn steady state; a round where a rollout file changed re-parses just that file, and one cold full-corpus pass runs per server process start:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.api.ext_usage import CodexUsageProvider, _list_rollout_files

prov = CodexUsageProvider("main", str(Path.home() / ".codex"))
rollouts = _list_rollout_files(prov.sessions_dir)
prov._compute_spend(rollouts)  # cold pass, as at a server restart; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    prov._compute_spend(rollouts)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{len(rollouts)} rollout files; steady-state spend rescan median {times[2]:.4f} s, max {times[-1]:.4f} s")
EOF
```

M10 — thread-metadata torn reads: the invariant behind the threads/list endpoint's writes.
`list_threads` validates every thread's `metadata.json` from an executor thread with no
coordination against `save_metadata`'s rewrite, so a save that publishes the file
truncated lets a concurrent poll observe a half-written file; that read fails
`ThreadMetadata` validation and 500s the whole list response (the same failure the
3 s workers-panel poll hits). The collector cannot drive that race through HTTP on the
live instance without writing to its state, so it reproduces the race against the main
checkout's code with scratch state: 3000 `save_metadata` calls on one thread's file
while four reader threads validate every read. A torn read under this stream is a read
the fixed write path cannot produce; the count is the metric.

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, tempfile, threading
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, ThreadMetadata
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

WRITES = 3000

async def main():
    work = Path(tempfile.mkdtemp(prefix="m10-torn-read-"))
    cfg = CharlieBotConfig(charliebot_home=work / "home")
    sessions = SessionManager(cfg)
    threads = ThreadManager(cfg)
    session = await sessions.create_session(CreateSessionRequest(name="M10"))
    meta = await threads.create_thread(session, "m10")
    path = work / "home" / "sessions" / session.id / "threads" / meta.id / "metadata.json"
    torn = 0
    reads = 0
    stop = False

    async def writer():
        for _ in range(WRITES):
            await threads.save_metadata(meta)

    def reader():
        nonlocal torn, reads
        while not stop:
            raw = path.read_text(encoding="utf-8")
            reads += 1
            try:
                ThreadMetadata.model_validate_json(raw)
            except ValueError:
                torn += 1

    running = [threading.Thread(target=reader) for _ in range(4)]
    for t in running:
        t.start()
    await writer()
    stop = True
    for t in running:
        t.join()
    print(f"{WRITES} save_metadata calls, {reads} concurrent reads; torn reads observed: {torn}")

asyncio.run(main())
EOF
```

M11 — backlog read endpoints' status codes. This host configures no
`backlog_repos`, so both GETs read the empty-state path, and every 500 arrives
with a ~30-line ASGI traceback in the server log (the line the 5xx count below
matches). Evidence while the live server runs older code is a scratch-instance
A/B (scratch `CHARLIEBOT_HOME`, TestClient against before and after code), the
same shape as the M7 protocol.

```bash
KEY=$(grep -m1 '^charliebot_access_key:' ~/.charliebot/config.yaml | awk '{print $2}'); for p in /api/backlog /api/backlog/history; do curl -s -o /dev/null -w "$p %{http_code}\n" -H "Authorization: Bearer $KEY" "http://127.0.0.1:18498$p"; done
LOG=$(ls -1t /tmp/charliebot-logs/server_*.log | head -1); grep -c "GET /api/backlog HTTP/1.1\" 500" "$LOG"
```

M12 — ext-usage poller codex usage scrape, steady state. The scrape reads each account's
newest rollout every round for the latest token_count event; the collector times the function
the poller calls each round over the live corpus (read-only), from the main repo checkout. An
unchanged file is the steady state; a changed file re-reads only its tail window (the
full-file read remains for a token_count farther back than the window):

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.api.ext_usage import CodexUsageProvider, _list_rollout_files

prov = CodexUsageProvider("main", str(Path.home() / ".codex"))
rollouts = _list_rollout_files(prov.sessions_dir)
prov._fetch_usage(rollouts)  # cold pass, as at a server restart; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    prov._fetch_usage(rollouts)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{len(rollouts)} rollout files; steady-state usage scrape median {times[2]:.4f} s, max {times[-1]:.4f} s")
EOF
```

M13 — thread-events read+transform, steady state. The workers panel polls
`GET /api/threads/{sid}/threads/{tid}/events` every 5 s for each expanded
running worker, and the endpoint projects the worker's whole events log on
every call. The cost is invisible to HTTP probes of the standing metrics, so
the collector times the function the handler calls over the largest on-disk
worker log (read-only), from the main repo checkout. An unchanged log is the
steady state (one stat); a log appended between calls re-parses only its new
tail; a shrunk log restarts its entry from scratch. Evidence while the live
server runs older code is a scratch-instance A/B, the same shape as the M7
protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.api.threads import read_thread_worker_events

root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
read_thread_worker_events(best)  # cold pass, as at first panel open; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    read_thread_worker_events(best)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{best_n / 1e6:.1f} MB thread log; steady-state read+transform median {times[2]:.4f} s, max {times[-1]:.4f} s")
EOF
```

M14 — git diff API event-loop lag. The diff endpoints run `git` subprocesses whose duration
is user data (up to SUBPROCESS_GIT_DIFF_TIMEOUT per call, several calls per request); run
inline in the async handler, that time is an event-loop freeze for every concurrent request
and WebSocket, invisible to the standing probes above. The collector drives the `diff_files`
handler over the charlie-bot checkout's full history (root commit .. HEAD — the largest
range this host's workspace carries) with a concurrent 5 ms ticker, and reports the worst
ticker gap per run; when the handler never yields, every gap is the handler's whole wall
time. Evidence while the live server runs older code points the same collector at the
branch checkout, the same shape as the M7 protocol (both runs are read-only against live
state: the scratch CHARLIEBOT_HOME is a tempfile):

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.api.git import diff_files

REPO = Path("/home/chaoli/workspace/charlie-bot")
BASE = subprocess.run(["git", "rev-list", "--max-parents=0", "HEAD"],
                      cwd=REPO, capture_output=True, text=True, check=True).stdout.splitlines()[0]
cfg = CharlieBotConfig(charliebot_home=Path(tempfile.mkdtemp(prefix="m14-home-")),
                       workspace_dirs=["/home/chaoli/workspace"])

async def run_once():
    gaps = []
    stop = False
    async def ticker():
        prev = time.perf_counter()
        while not stop:
            await asyncio.sleep(0.005)
            now = time.perf_counter()
            gaps.append(now - prev)
            prev = now
    t = asyncio.create_task(ticker())
    t0 = time.perf_counter()
    result = await diff_files(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot", cfg=cfg)
    wall = time.perf_counter() - t0
    stop = True
    await t
    return (max(gaps) if gaps else wall), result["total_files"]

async def main():
    await run_once()  # cold pass, as at first diff after a server start; not timed
    worst = []
    total = 0
    for _ in range(5):
        gap, total = await run_once()
        worst.append(gap)
    worst.sort()
    print(f"{total} files in root..HEAD diff; loop-lag median {worst[2]:.4f} s, max {worst[-1]:.4f} s")

asyncio.run(main())
EOF
```

M15 — recap-summary cache torn reads: the invariant behind the recap view's
cache writes. `get_session_recap` reads `recap_summaries.json` from an executor
thread with no coordination against `_write_cache_entry`'s rewrite from the
summarize handler, so a write that publishes the file truncated lets a
concurrent read observe a half-written file; that read fails json parsing and
500s the recap response. The collector reproduces the race against the main
checkout's code with scratch state: 3000 `_write_cache_entry` calls on one
session's cache file while four reader threads run the handler's lookup. A torn
read under this stream is a read the fixed write path cannot produce; the count
is the metric. Evidence while the live server runs older code points the same
collector at the branch checkout (`sys.path.insert` at the worktree root).

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, tempfile, threading
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core import recap

WRITES = 3000

async def setup():
    work = Path(tempfile.mkdtemp(prefix="m15-torn-read-"))
    cfg = CharlieBotConfig(charliebot_home=work / "home")
    sessions = SessionManager(cfg)
    session = await sessions.create_session(CreateSessionRequest(name="M15"))
    return work, sessions, session

work, sessions, session = asyncio.run(setup())
recap._write_cache_entry(sessions, session.id, 0, "seed")

state = {"torn": 0, "reads": 0, "stop": False, "errors": []}

def writer():
    for i in range(WRITES):
        recap._write_cache_entry(sessions, session.id, i % 100, f"summary {i}")

def reader():
    while not state["stop"]:
        state["reads"] += 1
        try:
            recap.lookup_cached_summary(sessions, session.id, 0)
        except ValueError:
            state["torn"] += 1
        except BaseException as e:
            state["errors"].append(repr(e))
            return

running = [threading.Thread(target=reader) for _ in range(4)]
for t in running:
    t.start()
writer()
state["stop"] = True
for t in running:
    t.join()
if state["errors"]:
    raise SystemExit(f"unexpected reader errors: {state['errors'][:3]}")
print(f"{WRITES} _write_cache_entry calls, {state['reads']} concurrent reads; torn reads observed: {state['torn']}")
EOF
```

M16 — trigger-file torn reads: the invariant behind the trigger files the polls read.
`_save_trigger` rewrites a trigger's JSON on schedule/cancel/fire while
`list_triggers` (the 3 s workers-panel poll, the session view) and the sidebar
probe (`pending_trigger_state_sync`) read it from executor threads with no
coordination; a save that publishes the file truncated lets a concurrent read
observe a half-written file, fail JSON parsing, and drop the trigger from that
poll's list and pending count. The collector reproduces the race against the
checkout's code with scratch state: 3000 `_save_trigger` calls on one trigger's
file while four reader threads run the read path (`read_text` +
`_migrate_legacy_watch_pids`). A torn read under this stream is a read the
fixed write path cannot produce; the count is the metric. Evidence while the
live server runs older code points the same collector at the branch checkout
(`sys.path.insert` at the worktree root).

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, tempfile, threading
from datetime import UTC, datetime, timedelta
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.models import PendingTrigger
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager, _migrate_legacy_watch_pids

WRITES = 3000

work = Path(tempfile.mkdtemp(prefix="m16-torn-read-"))
cfg = CharlieBotConfig(charliebot_home=work / "home")
sessions = SessionManager(cfg)
triggers = TriggerManager(cfg, sessions)
trigger = PendingTrigger(
    session_id="m16",
    fire_at=datetime.now(UTC) + timedelta(hours=1),
    message="m16 torn-read probe",
    watch_targets=[],
)
path = cfg.sessions_dir / "m16" / "triggers" / f"{trigger.id}.json"

state = {"torn": 0, "reads": 0, "stop": False, "errors": []}

async def writer():
    for _ in range(WRITES):
        await triggers._save_trigger(trigger)

def reader():
    while not state["stop"]:
        raw = path.read_text(encoding="utf-8")
        state["reads"] += 1
        try:
            _migrate_legacy_watch_pids(raw)
        except ValueError:
            state["torn"] += 1
        except BaseException as e:
            state["errors"].append(repr(e))
            return

async def main():
    await triggers._save_trigger(trigger)
    running = [threading.Thread(target=reader) for _ in range(4)]
    for t in running:
        t.start()
    await writer()
    state["stop"] = True
    for t in running:
        t.join()

asyncio.run(main())
if state["errors"]:
    raise SystemExit(f"unexpected reader errors: {state['errors'][:3]}")
print(f"{WRITES} _save_trigger calls, {state['reads']} concurrent reads; torn reads observed: {state['torn']}")
EOF
```

M17 — session fork (clone) latency: the endpoint behind `POST /api/sessions/{id}/fork`,
`SessionManager.fork_session`, parses the parent's archived and live chat events and
re-serializes them into the child's `data/parent_reference.jsonl`, appends the clone event,
and copies plan artifacts file by file, so the wall time scales with the parent's chat-event
bytes while the `threads/` payload — often the bulk of a session's directory size — is never
read. A fork writes state, so the live instance cannot be probed read-only; the collector
resolves the session with the heaviest fork corpus (live chat events plus archives), copies
only that session into a scratch `CHARLIEBOT_HOME` under /tmp, and times `fork_session` from
the main checkout, one cold pass then five timed forks. Evidence while the live server runs
older code points the same collector at the branch checkout (`sys.path.insert` at the
worktree root), the same shape as the M15 protocol.

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Heaviest fork corpus: live chat events plus archives, the bytes fork_session parses
# and re-serializes into the child's parent_reference.jsonl.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    data = d / "data"
    corpus = [data / "chat_events.jsonl", *sorted((data / "archives").glob("chat_events.*.jsonl"))]
    n = sum(p.stat().st_size for p in corpus if p.is_file())
    if n > best_n:
        best, best_n = d, n
SID = best.name
print(f"heaviest fork corpus: session {SID}, {best_n / 1e6:.1f} MB chat events")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that session; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m17-fork-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.parent.mkdir(parents=True)
shutil.copytree(best, dst)

cfg = CharlieBotConfig(charliebot_home=home)
sessions = SessionManager(cfg)

async def main():
    await sessions.fork_session(SID)  # cold pass, as at first fork after a server start; not timed
    times = []
    for _ in range(5):
        t0 = time.perf_counter()
        await sessions.fork_session(SID)
        times.append(time.perf_counter() - t0)
    times.sort()
    n = sessions.get_chat_event_count_sync(SID)
    print(f"{n} parent events; fork median {times[2]:.4f} s, max {times[-1]:.4f} s over 5 runs")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M18 — hidden-tab periodic poll fetches: the invariant behind page-timers.js
("a hidden tab does no periodic work at all"). A poll routined around the
registry keeps fetching while the tab is hidden (browser throttling slows but
never stops a raw interval). The collector loads the checkout's real
page-timers.js, workers.js and ext_usage.js in a node vm with a stub document,
holds the page hidden, starts one expanded running worker's thread-detail poll
plus the ext-usage strip's DOMContentLoaded init, and fires every registered
interval the number of times its cadence fits a simulated 10 minutes. Fetches
issued after bootstrap settle are the metric; healthy is 0. The closing 10 s
visible re-check must fetch again (a poll that never resumes is a finding, not
a pass). Evidence while the live server runs older code points the same
collector at the branch checkout (`CHECKOUT` at the worktree root):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} node - <<'EOF'
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const CHECKOUT = process.env.CHECKOUT || '/home/chaoli/workspace/charlie-bot';
const read = (name) => fs.readFileSync(path.join(CHECKOUT, 'web/static/js', name), 'utf8');

const listeners = new Map();
const elements = new Map([
  ['thread-detail-t1', {classList: {contains: () => false}}],
  ['thread-dot-t1', {classList: {contains: (c) => c === 'bg-blue-500'}}],
  ['thread-events-t1', {innerHTML: '', parentElement: {querySelector: () => null, insertBefore() {}}}],
]);
const documentStub = {
  hidden: true,
  addEventListener(type, fn) {
    if (!listeners.has(type)) listeners.set(type, []);
    listeners.get(type).push(fn);
  },
  removeEventListener() {},
  dispatch(type) {
    (listeners.get(type) || []).forEach((fn) => fn());
  },
  getElementById: (id) => elements.get(id) || null,
  querySelector: () => null,
  querySelectorAll: () => [],
  createElement: () => ({style: {}, classList: {add() {}, remove() {}, toggle() {}}, setAttribute() {}}),
  body: {appendChild() {}, style: {}},
};

const intervals = new Map();
let nextId = 1;
let fetches = 0;
const context = {
  document: documentStub,
  setInterval(fn, ms) {
    const id = nextId++;
    intervals.set(id, {id, fn, ms});
    return id;
  },
  clearInterval(id) {
    intervals.delete(id);
  },
  console: {error() {}, warn() {}, log() {}},
  fetch: async (url) => {
    fetches += 1;
    return {ok: true, json: async () => []};
  },
  localStorage: {getItem: () => null, setItem() {}, removeItem() {}},
};
vm.createContext(context);
vm.runInContext(read('page-timers.js'), context, {filename: 'page-timers.js'});
vm.runInContext(read('workers.js'), context, {filename: 'workers.js'});
vm.runInContext(read('ext_usage.js'), context, {filename: 'ext_usage.js'});

async function tickWindow(seconds) {
  for (const entry of Array.from(intervals.values())) {
    const n = Math.floor((seconds * 1000) / entry.ms);
    for (let i = 0; i < n; i++) await entry.fn();
  }
}

(async () => {
  documentStub.dispatch('DOMContentLoaded');
  await new Promise((r) => setImmediate(r));
  context.startThreadPoll('t1', 'session-a');
  await new Promise((r) => setImmediate(r));
  const bootstrapFetches = fetches;

  fetches = 0;
  await tickWindow(600);
  const hiddenFetches = fetches;

  documentStub.hidden = false;
  documentStub.dispatch('visibilitychange');
  fetches = 0;
  await tickWindow(10);
  const visibleFetches = fetches;

  console.log(
    `checkout ${CHECKOUT}: ${hiddenFetches} poll fetches per simulated 10 hidden min ` +
    `(${bootstrapFetches} bootstrap fetch excluded); ${visibleFetches} fetches in the 10 s visible re-check`
  );
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
EOF
```

M19 — SSE line framing of a chunked large-frame stream. `iter_sse_lines` frames
both opencode master/worker event streams and the anthropic-proxy upstream, and
its terminator-search cost is invisible to the HTTP probes above: a frame spans
as many byte chunks as the network delivers, so when the search re-scans the
accumulated remainder on every chunk, framing costs O(bytes × chunks) — seconds
of blocked event-loop time per multi-MB tool payload at network chunk sizes —
while a resumable search stays O(bytes). The collector streams a 16 MB payload
of ~1 MB frames (terminators at frame ends only) through the adapter at a fixed
16 KB chunking against the checkout's code, synthetic and read-only. Evidence
points the same collector at the before and after checkouts
(`sys.path.insert` at each root), the same shape as the M7 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.sse import iter_sse_lines

class _Stub:
  def __init__(self, chunks):
    self._chunks = chunks
  async def aiter_bytes(self):
    for c in self._chunks:
      yield c

frame = b"data: " + b"x" * 1000000 + b"\n\n"
payload = frame * 16
chunks = [payload[i:i + 16 * 1024] for i in range(0, len(payload), 16 * 1024)]

async def run_once():
  lines = 0
  t0 = time.perf_counter()
  async for _ in iter_sse_lines(_Stub(chunks)):
    lines += 1
  return time.perf_counter() - t0, lines

async def main():
  await run_once()  # cold pass, as at a first big frame after a server start; not timed
  times = []
  lines = 0
  for _ in range(5):
    dt, lines = await run_once()
    times.append(dt)
  times.sort()
  print(f"{len(payload) / 1e6:.1f} MB payload in 16 KB chunks, ~1 MB frames, {lines} lines; "
        f"framing median {times[2]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M20 — recap extract repeats at one divider. `GET /api/sessions/{id}/recap?upto=…`
runs `extract_recap`, which parses and projects every event below the divider; the
chat UI re-requests an open recap panel on every re-materialization (virtualized
scrolling, turn re-renders), so each repeat pays a full corpus scan for an
unchanged result and the cost is invisible to the standing HTTP probes. The
collector copies the worst on-disk extract corpus (the session whose live chat
file plus archives carry the most bytes — the corpus extraction parses;
metadata.json plus data/ only) into a scratch `CHARLIEBOT_HOME` under /tmp and
times `extract_recap` from the checkout, one cold pass then five timed repeats at
the same divider — the re-materialization pattern, identical results which the
memo serves without a re-scan. Evidence while the live server runs older code
points the same collector at the branch checkout (`sys.path.insert` at the
worktree root), the same shape as the M15 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core import recap

# Worst extract corpus: the session whose chat events (live file plus archives)
# carry the most bytes; extract_recap parses every event below the divider.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    data = d / "data"
    corpus = [data / "chat_events.jsonl", *sorted((data / "archives").glob("chat_events.*.jsonl"))]
    n = sum(p.stat().st_size for p in corpus if p.is_file())
    if n > best_n:
        best, best_n = d, n
SID = best.name
print(f"worst extract corpus: session {SID}, {best_n / 1e6:.1f} MB chat events")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m20-recap-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
count = mgr.get_chat_event_count_sync(SID)
upto = count - 1

recap.extract_recap(mgr, SID, upto)  # cold pass, as at first divider open; not timed
times = []
result = None
for _ in range(5):
    t0 = time.perf_counter()
    result = recap.extract_recap(mgr, SID, upto)
    times.append(time.perf_counter() - t0)
times.sort()
digest = hashlib.sha256(json.dumps(result, sort_keys=True).encode()).hexdigest()[:12]
print(f"{count} events, {len(result['asks'])} asks, digest {digest}; "
      f"repeat-divider extract median {times[2]:.4f} s, max {times[-1]:.4f} s")
shutil.rmtree(home)
EOF
```

M21 — sidebar 10th-poll probe sweep, steady state. Every 10th `/api/sessions/status`
poll re-selects every active session for a sidebar re-probe (the self-heal window for
a writer that forgot its dirty mark). The pre-fix sweep deep-probed all of them — a
scandir+stat plus a read+parse of every 30-day-window thread metadata, every trigger
file, and every plans.json — where the fixed sweep pays one stat-only signature pass
per session and deep-probes only sessions whose probe inputs changed (the escape
hatch `/status?force=1` still deep-probes everything). The cost is background work
invisible to HTTP probes, so the collector times the sweep function the poll awaits
(read-only over the live state), with the poll's on-loop signature storage replayed
between sweeps. The steady state is no probe input changed between sweeps; the cold
pass, as at a server start with no signatures stored, is a full deep probe and is not
timed. The pre-fix number used in the landing PR's evidence is the same sweep-shaped
command against `probe_sidebar_state_sync` unconditionally (the poll's pre-fix call).

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core import sidebar_state
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager, selective_probe_sidebar_state

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    metas = await asyncio.to_thread(mgr.list_active_session_metas)
    specs = [
        (m.id, mgr._threads_dir(m.id), mgr._session_dir(m.id) / "triggers", mgr._session_dir(m.id) / "plans.json")
        for m in metas
    ]
    sidebar_state.reset_for_tests()

    def sweep():
        entries, sigs = selective_probe_sidebar_state(specs, deep=False)
        for sid, sig in sigs.items():
            sidebar_state.store_probe_signature(sid, sig)  # the poll's on-loop storage half
        return entries

    sweep()  # cold pass, as at a server start with no signatures stored; not timed
    times = []
    for _ in range(5):
        t0 = time.perf_counter()
        sweep()
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"{len(specs)} active sessions; steady-state sidebar sweep median {times[2]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M22 — ext-usage unknown-limit-shape warning stream, steady state. Every account
fetch re-runs the response transform (`_transform_response` with
`_scoped_windows`, `_transform_codex_response` with `_codex_windows`), and each
unrecognized utilization-bearing field or unmappable scoped entry logs
`ext_usage_unknown_limit_shape`; a field the upstream keeps sending re-fires the
same warning every fetch round forever — 1536 lines in the 20.47 h live server
log sampled 2026-09-01 (~75/h, 20 % of the log's structured lines) — while one
sighting per process carries the whole signal. The cost is background log volume
invisible to HTTP probes, so the collector drives the pure transform directly
over a synthetic response shaped like the observed one (two unknown top-level
utilization fields plus one scoped entry missing its percent): one first-sighting
call, as at a process start, then 60 steady-state repeat rounds, counting the
event. A warning in the repeat window is a re-fired alarm; the count is the
metric. Evidence while the live server runs older code points the same collector
at the branch checkout (`sys.path.insert` at the worktree root), the same shape
as the M7 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.api import ext_usage as ext_usage_mod

raw = {
    "fiveHour": {"utilization": 11.0, "resetsAt": "2026-08-04T12:00:00+00:00"},
    "sevenDay": {"utilization": 22.0, "resetsAt": "2026-08-04T19:00:00+00:00"},
    "nimbus_quill": {"utilization": 3.0, "resetsAt": "2026-08-04T20:00:00+00:00"},
    "extra_usage": {"utilization": 5.0, "resets_at": "2026-08-05T00:00:00+00:00"},
    "limits": [
        {"kind": "weekly_scoped", "group": "weekly", "resets_at": "",
         "scope": {"model": {"display_name": "Nimbus"}}},
    ],
}

warns = []
orig = ext_usage_mod.log.warning
ext_usage_mod.log.warning = lambda event, **kw: warns.append({"event": event, **kw})
try:
    ext_usage_mod._transform_response(raw, account="main")  # first sighting, as at a process start; not counted
    warns.clear()
    for _ in range(60):  # steady-state repeat rounds of the poll's re-transform
        ext_usage_mod._transform_response(raw, account="main")
finally:
    ext_usage_mod.log.warning = orig
n = sum(1 for w in warns if w["event"] == "ext_usage_unknown_limit_shape")
print(f"60 steady-state transform rounds; ext_usage_unknown_limit_shape warnings: {n}")
EOF
```

M23 — archive-range chat-event rescan, steady state. Sessions with
`archive_offset > 0` paginate backwards through `load_chat_events_range`,
whose archive half re-read and re-scanned every archive file below the page
end on every page click (and on every cold recap extract touching archives).
The fixed reader memoizes each archive file's parsed events on
(mtime_ns, size): a repeat range read over unchanged archives pays one stat
per file and zero corpus bytes, mirroring the live events cache that
unarchived sessions already serve from. Archive files are append-only within
their week and frozen after, so the key is sound; a same-week recycle append
re-parses only that file. The cost is invisible to HTTP probes of archived
sessions (deep page turns only), so the collector copies the session whose
`data/archives` carries the most bytes into a scratch `CHARLIEBOT_HOME`
under /tmp (metadata.json and data/ only; live home read once for the copy,
never written), warms the metadata cache as the live server's polls do, and
times 8-page backwards scrolls of 200 events below the archive end — one
cold pass, as at first scroll after a server start with an empty memo, then
five timed repeats. Evidence while the live server runs older code points
the same collector at the branch checkout (`sys.path.insert` at the worktree
root), the same shape as the M15 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst archived corpus: the session whose data/archives carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    arch = d / "data" / "archives"
    if arch.is_dir():
        n = sum(p.stat().st_size for p in arch.glob("chat_events.*.jsonl"))
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst archived corpus: session {SID}, {best_n / 1e6:.1f} MB in archives")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m23-arch-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
asyncio.run(mgr.get_session(SID))  # warm the metadata cache, as the live server's polls do
offset = mgr._chat_events.read_archive_offset_sync(SID)

def scroll():
    before = offset
    for _ in range(8):
        if before <= 0:
            break
        mgr.load_chat_events_range(SID, max(0, before - 200), before)
        before -= 200

scroll()  # cold pass, as at first scroll after a server start; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    scroll()
    times.append(time.perf_counter() - t0)
times.sort()
print(f"archive_offset {offset}; 8-page scroll steady-state median {times[2]:.4f} s, max {times[-1]:.4f} s")
shutil.rmtree(home)
EOF
```

M24 — trigger list, steady state. The 3 s workers-panel poll
(`GET /api/threads/{sid}/list`) and the session view render call
`TriggerManager.list_triggers`, whose pre-fix form read and parsed every
trigger file of the session on every call. The cost is invisible to the
standing HTTP probes (panels poll the sessions they have open, not the worst
corpus), so the collector resolves the session whose triggers directory
carries the most files and times the manager function the poll awaits
(read-only over the live state), from the main repo checkout: one cold pass,
as at a server start with an empty memo, then five timed calls. The fixed
reader memoizes each trigger file's parsed record on (mtime_ns, size): the
steady state is one scandir and zero corpus bytes, and a `_save_trigger`
rewrite (schedule/cancel/fire) re-reads only that file. Trigger files change
only through that atomic rewrite, so the key is sound. Evidence while the
live server runs older code points the same collector at the branch checkout
(`sys.path.insert` at the worktree root), the same shape as the M7 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, time
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager

# Worst trigger corpus: the session whose triggers directory carries the most files.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.glob("*/triggers"):
    n = sum(1 for p in d.glob("*.json") if p.is_file())
    if n > best_n:
        best, best_n = d, n
SID = best.parent.name

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    triggers = TriggerManager(cfg, SessionManager(cfg))
    await triggers.list_triggers(SID)  # cold pass, as at a server start; not timed
    times = []
    result = None
    for _ in range(5):
        t0 = time.perf_counter()
        result = await triggers.list_triggers(SID)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"session {SID}, {len(result)} triggers (worst corpus {best_n} files); "
          f"steady-state list_triggers median {times[2]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M25 — scheduler config reload, steady state. Every 60 s scheduler tick re-reads the
config so edited cron tasks take effect without a restart; the pre-fix form ran a full
YAML parse and model validation inline on the event loop on every tick, while the fixed
form routes through the process-wide fingerprint-cached `get_config` — one stat-key
comparison when nothing changed, a real reload only on a fingerprint change (which still
lands within one tick, the same freshness the per-tick read guaranteed). Ticks are
background work invisible to HTTP probes, so the collector drives
`Scheduler._reload_config` over the live config corpus (read-only: a parse, never a
write) with a concurrent 5 ms ticker, from the checkout under test: one cold pass, as
at a server start, then five timed steady-state reloads (a reload faster than the
ticker cadence reports its own wall time). Evidence while the live server runs older
code points the same collector at the branch checkout (`CHECKOUT` at the worktree
root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from unittest.mock import AsyncMock
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import get_config
from src.core.scheduler import Scheduler

async def main():
    sched = Scheduler(get_config(), AsyncMock())
    sched._reload_config()  # cold pass, as at a server start; not timed
    worst = []
    for _ in range(5):
        gaps = []
        stop = False
        async def ticker():
            prev = time.perf_counter()
            while not stop:
                await asyncio.sleep(0.005)
                now = time.perf_counter()
                gaps.append(now - prev)
                prev = now
        t = asyncio.create_task(ticker())
        t0 = time.perf_counter()
        sched._reload_config()
        wall = time.perf_counter() - t0
        stop = True
        await t
        worst.append(max(gaps) if gaps else wall)
    worst.sort()
    print(f"steady-state scheduler config reload loop-lag median {worst[2]:.4f} s, max {worst[-1]:.4f} s")

asyncio.run(main())
EOF
```

M26 — message-projection advance per appended event. The chat bootstrap
(`GET /api/sessions/{id}/bootstrap`, session view, SPA switches) and the
events pagination endpoint serve from the per-session message projection;
every one of those reads on a session whose live chat file grew since the
last read re-derived the projection from scratch — a full re-aggregation of
every event, ~59 ms on the 20534-event worst live corpus, per SPA switch or
page click on an appending session. The fixed projection is append-
incremental: `stable_closed_prefix_len` splits the raw stream at the last
point no open OpenCode run interval crosses, the closed prefix feeds the
aggregator exactly once, and the still-open tail is re-evaluated per call
through a cloned aggregator, so an advance costs O(open tail) instead of
O(history) while the served view still equals `events_to_view(all_events)`.
The split rule is sound because the stable-history projection defers queued
users only within one completed run interval; a shrinking live file (rewind)
still pays a full rebuild. The cost is a per-interaction latency no standing
probe sees, so the collector copies the session with the most live chat
events into a scratch `CHARLIEBOT_HOME` under /tmp (metadata.json and data/
only; live home read once for the copy, never written), builds the
projection once (cold pass, as at first view after a server start; not
timed), then appends one event at a time and times the per-append advance,
checking the served view against the whole-list reference digest. Evidence
while the live server runs older code points the same collector at the
branch checkout (`CHECKOUT` at the worktree root), the same shape as the
M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.api.message_utils import events_to_messages

# Worst projection corpus: the session whose LIVE chat file carries the most
# events (archive_offset > 0 sessions take the legacy path, never the projection).
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        with open(p, errors="replace") as f:
            n = sum(1 for _ in f)
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst projection corpus: session {SID}, {best_n} live chat events")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m26-proj-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)

async def main():
    mgr.get_message_projection(SID)  # cold pass, as at first view after a server start; not timed
    times = []
    for i in range(8):
        await mgr.save_chat_event(SID, {
            "id": f"m26-probe-{i}", "type": "assistant",
            "message": {"content": [{"type": "text", "text": f"probe chunk {i}"}]},
            "timestamp": f"2026-09-01T19:00:{i:02d}Z",
        })
        t0 = time.perf_counter()
        projection = mgr.get_message_projection(SID)
        times.append(time.perf_counter() - t0)
    times.sort()
    all_events = mgr.load_chat_events_sync(SID)
    def digest(msgs):
        ident = [(m.get("id"), m.get("role"), len(m.get("content", "") or ""), m.get("event_index")) for m in msgs]
        return hashlib.sha256(json.dumps(ident).encode()).hexdigest()[:12]
    ref = events_to_messages(all_events)
    match = digest(projection.history) == digest(ref)
    print(f"8 single-event appends on {best_n}-event corpus; projection advance "
          f"median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms; parity {match} (digest {digest(projection.history)})")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M27 — plans registry tolerant read, steady state. The plan panel's poll
endpoint (`GET /api/sessions/{id}/plans`) and the sidebar deep probe both run
`read_plans_tolerant`, whose pre-fix form read and parsed the session's
plans.json and re-derived every plan's state on every call. The cost is
invisible to the standing HTTP probes (the panel polls the session it has
open, not the worst corpus), so the collector times the function over the
session whose plans.json carries the most bytes (read-only over the live
state), from the checkout under test: one cold pass, as at a server start
with an empty memo, then five timed calls. The fixed reader memoizes each
plans.json's projected result on (mtime_ns, size): the steady state is one
exists+stat pair and zero registry bytes, and a verb's rewrite re-reads only
that file. Registry writes go through `write_json_atomically` and the derived
state is a pure function of the file content, so the key is sound. Evidence
while the live server runs older code points the same collector at the branch
checkout (`CHECKOUT` at the worktree root), the same shape as the M7
protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.plans import read_plans_tolerant

# Worst plans corpus: the session whose plans.json carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "plans.json"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = p, n
print(f"worst plans corpus: session {best.parent.name}, {best_n / 1e3:.1f} KB plans.json")

result = read_plans_tolerant(best, best.parent.name)  # cold pass, as at a server start with an empty memo; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    result = read_plans_tolerant(best, best.parent.name)
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{len(result['plans'])} plans, {len(result['errors'])} errors; steady-state tolerant read "
      f"median {times[2] * 1e6:.1f} us, max {times[-1] * 1e6:.1f} us")
EOF
```

M28 — ndjson tail+count scan, steady state. The chat paging paths for sessions
without a message projection (``archive_offset > 0`` — bootstrap, view, and events
pages) call ``parse_ndjson_tail`` on every request, and its count half must scan
the whole live file to report ``total_line_count`` (``count_ndjson_lines`` is the
same scan for the recap default divider and the session GET). The pre-fix form
walked the file line by line in Python — and even ``bytes.count`` measures only
~0.7 GB/s on this host — while the fixed form counts newlines per 1 MiB chunk
through a numpy SIMD compare (~3.4 GB/s measured), keeping the file-iteration
count contract (an unterminated final line counts) that the tail result's global
ordinal math depends on. The cost is per page view, invisible to the standing
HTTP probes, so the collector times both functions over the largest live chat
file on disk (read-only), from the checkout under test: one cold pass, as at a
first page view after a server start, then seven timed calls. Evidence while the
live server runs older code points the same collector at the branch checkout
(``CHECKOUT`` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.ndjson import parse_ndjson_tail, count_ndjson_lines

# Worst tail+count corpus: the session whose LIVE chat file carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "data" / "chat_events.jsonl"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = p, n

parse_ndjson_tail(best, 200)  # cold pass, as at a first page view after a server start; not timed
times = []
for _ in range(7):
    t0 = time.perf_counter()
    events, total, has_more = parse_ndjson_tail(best, 200)
    times.append(time.perf_counter() - t0)
times.sort()
ctimes = []
for _ in range(7):
    t0 = time.perf_counter()
    n = count_ndjson_lines(best)
    ctimes.append(time.perf_counter() - t0)
ctimes.sort()
print(f"{best_n / 1e6:.1f} MB file, {total} lines, tail events {len(events)}; "
      f"parse_ndjson_tail median {times[3] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms; "
      f"count_ndjson_lines median {ctimes[3] * 1000:.2f} ms")
EOF
```

M29 — session-metadata listing preamble, steady state. The status poll
(`GET /api/sessions/status`), the sessions list, the archived pages, and search
all route through `_load_session_metas`, whose first step lists the session
directories; this host's sessions root holds ~1000 dirs, so the pre-fix
`Path.iterdir()` + `is_dir()` form rebuilt a Path per entry and paid one stat()
each (~6 ms measured), while the fixed `os.scandir` pass answers `is_dir()` from
the directory record itself (~1 ms). The cost is a slice of every listing call
and stays invisible to the standing HTTP probes (a poll's total keeps its own
budget), so the collector times the manager function the listings await
(read-only over the live state), from the checkout under test: one cold pass,
as at a server start with an empty metadata cache, then nine timed calls. The
steady state is every metadata cache entry fresh (TTL-fresh or archived), so a
call pays the dir scan plus cache lookups and zero file reads. Evidence while
the live server runs older code points the same collector at the branch
checkout (`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.models import SessionStatus
from src.core.sessions import SessionManager

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    await mgr._load_session_metas()  # cold pass, as at a server start with an empty metadata cache; not timed
    times = []
    n = 0
    for _ in range(9):
        t0 = time.perf_counter()
        metas = await mgr._load_session_metas(SessionStatus.ACTIVE)
        times.append(time.perf_counter() - t0)
        n = len(metas)
    times.sort()
    print(f"{n} active sessions in listing; steady-state _load_session_metas "
          f"median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms")

asyncio.run(main())
EOF
```

M30 — live-half chat-event range rescan, steady state. Archived sessions
(``archive_offset > 0``) paginate backwards through ``load_chat_events_range``,
whose live half re-read the live file from byte 0 on every page click (and on
every cold recap extract touching the live tail) while the archive half already
served repeats from the M23 memo. The fixed reader memoizes the live file's
per-physical-line parsed events on (mtime_ns, size) for archived sessions —
the range index domain is physical lines (blank and malformed lines consume an
index, mirroring ``parse_ndjson_range``) — so a repeat scroll over an unchanged
live file pays one stat per page and zero corpus bytes; an append re-parses
once. The memo is gated to archived sessions: unarchived ones paginate through
the M26 message projection, so their range callers never reuse a moving live
corpus. The cost is invisible to HTTP probes of archived sessions (deep page
turns only), so the collector copies the archived session whose live
``chat_events.jsonl`` carries the most bytes into a scratch
``CHARLIEBOT_HOME`` under /tmp (metadata.json and data/ only; live home read
once for the copy, never written), warms the metadata cache as the live
server's polls do, and times 8-page backwards scrolls of 200 events over the
live half — one cold pass, as at first scroll after a server start with an
empty memo, then five timed repeats. Evidence while the live server runs older
code points the same collector at the branch checkout (``CHECKOUT`` at the
worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst live-half range corpus: the archived session whose LIVE chat file
# carries the most bytes; its live-half range reads re-parse per page click.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    meta_p = d / "metadata.json"
    if not meta_p.is_file():
        continue
    try:
        off = json.loads(meta_p.read_text()).get("archive_offset", 0)
    except Exception:
        continue
    live = d / "data" / "chat_events.jsonl"
    if off and off > 0 and live.is_file():
        n = live.stat().st_size
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst live-half range corpus: session {SID}, live {best_n / 1e6:.1f} MB")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m30-live-half-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
asyncio.run(mgr.get_session(SID))  # warm the metadata cache, as the live server's polls do
offset = mgr._chat_events.read_archive_offset_sync(SID)
total = mgr.get_chat_event_count_sync(SID)

def scroll():
    before = total
    for _ in range(8):
        if before <= offset:
            break
        mgr.load_chat_events_range(SID, max(offset, before - 200), before)
        before -= 200

scroll()  # cold pass, as at first scroll after a server start with an empty memo; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    scroll()
    times.append(time.perf_counter() - t0)
times.sort()
print(f"archive_offset {offset}, total {total}; 8-page live-half scroll steady-state "
      f"median {times[2]:.4f} s, max {times[-1]:.4f} s")
shutil.rmtree(home)
EOF
```

M31 — worker-finalize events-summary read, steady state. Every worker completion
runs `read_events_summary` on the finalize path (and again on the
reviewer-completion path for the original worker's log), quoting the log's last
parseable events into the worker_summary bubble. The pre-fix reader full-parsed
the whole events.jsonl (~41 ms measured on the 6.7 MB worst on-disk log) and
sliced the last 80; the fixed reader walks 512 KiB segments from the end
collecting the last 80 parseable events — identical output, blank and malformed
lines never counting toward the budget in either form. The cost is thread-pool
time invisible to HTTP probes, so the collector times the function the finalize
path awaits over the largest on-disk worker log (read-only), from the checkout
under test: one cold pass, as at first finalize after a server start, then five
timed calls. Evidence while the live server runs older code points the same
collector at the branch checkout (`sys.path.insert` at the worktree root), the
same shape as the M7 protocol. The sibling review-context scan (the reviewer
prompt's delegation lookup over the session chat log) moved from a full parse
to stream-until-first-match in the same change; its position-dependent numbers
ride along in the PR's Evidence section instead of carrying a standing row.

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, time
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from pathlib import Path
from src.core.config import CharlieBotConfig
from src.core.threads import ThreadManager
from src.core.spawner_events import read_events_summary

root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
SID, TID = best.parts[-5], best.parts[-3]

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    thread_mgr = ThreadManager(cfg)
    result = await read_events_summary(SID, TID, thread_mgr)  # cold pass, as at first finalize after a server start; not timed
    times = []
    for _ in range(5):
        t0 = time.perf_counter()
        result = await read_events_summary(SID, TID, thread_mgr)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"{best_n / 1e6:.1f} MB worker log, session {SID} thread {TID}; "
          f"steady-state events-summary read median {times[2]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M32 — memory-store assemble, steady state. The master run builds the
instruction block on every user message (``master_cc_run`` via
``_build_instructions_content``), and every worker spawn calls
``assemble_worker``; both full-parse the whole memory store (~68 entry files
plus the topics vocabulary) through ``load_store`` on every call. The cost is
to_thread time invisible to HTTP probes, so the collector times
``assemble_master`` over the live memory corpus (read-only), from the
checkout under test: one cold pass, as at a server start, then nine timed
calls. The fixed loader memoizes the parsed Store on a stat-only signature
((relative path, mtime_ns, size) of every parsed file): the steady state is
~69 stats and zero store bytes, and any rewrite, append, new entry, or
deletion re-parses. Only successful loads memoize — a malformed store keeps
raising on every call. Evidence while the live server runs older code points
the same collector at the branch checkout (``CHECKOUT`` at the worktree
root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.memory import assemble_master

memory_dir = Path.home() / ".charliebot" / "memory"
assemble_master(memory_dir)  # cold pass, as at a server start; not timed
times = []
for _ in range(9):
    t0 = time.perf_counter()
    assemble_master(memory_dir)
    times.append(time.perf_counter() - t0)
times.sort()
n_files = sum(1 for p in (memory_dir / "entries").rglob("*.md"))
print(f"{n_files} entry files; steady-state assemble_master "
      f"median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms")
EOF
```

M33 — assistant-stream draft render, full-turn replay. Every `stream` WebSocket
delta carries the whole accumulated draft, and the pre-fix `showStreaming`
(usage.js) painted it on every delta — `marked.parse(fixNestedFences(whole
draft))` plus a KaTeX re-walk and a bubble DOM swap — so a turn with N deltas
over an S-byte draft costs O(N × S) of main-thread markdown work, the chat UI's
jank source during long streamed turns; the fixed form coalesces paints to a
200 ms leading+trailing cadence, and `hideStreaming` cancels the pending paint
(every terminal path — committed bubble, error, session swap — hides first).
The cost is client-side, invisible to every HTTP probe, so the collector
(tests/stream_render_collector.js + the shared stream_render_harness.js) replays
a full turn in a node vm against the checkout's real usage.js/markdown-renderer.js
and the page-pinned marked build: the largest single assistant text block across
live chat files (a block cannot exceed its file's size, so smaller files skip),
fed as 200 B deltas at a 40 ms virtual cadence against a stub DOM/clock, one
cold pass then five timed replays wall-clocking only render work; KaTeX's
per-paint re-walk is stubbed identically in both arms, understating rather than
overstating the win (parity: the last frame equals a direct full-draft render).
Evidence points the collector at the branch checkout (`CHECKOUT` at the
worktree root, live state read-only), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} node /home/chaoli/workspace/charlie-bot/tests/stream_render_collector.js
```

## Sampling history

| Date | PR | Before → after | Note |
| --- | --- | --- | --- |
| 2026-08-30 | #457 | M5 median 0.068 s → 0.029 s (117-thread worst session) | one executor hop for the threads metadata scan; M5 definition and healthy range introduced with this PR |
| 2026-08-30 | #461 | M2 tui/status share 1396/3146 status polls (44 %) → 0 tui/status requests in a 15 s headless-page window (scratch A/B) | sidebar tui/status poll scoped to rows rendered as tui-cli; zero tui-cli backends configured on this host |
| 2026-08-30 | #463 | M6 median 0.015 s → 0.011 s (20534-event worst session, live-before vs scratch-after) | one-pass event scan for usage resolution; M6 definition and healthy range introduced with this PR |
| 2026-08-30 | #468 | M7 median 10.269 s → 2.087 s (live-before vs scratch-after warm; scratch cold 11.17 s; 2.2 GB agent logs) | persisted per-file Claude tally cache keyed on (mtime, size); M7 definition and healthy range introduced with this PR |
| 2026-08-30 | #476 | M8 median 0.269 s → 0.309 s, max 1.162 s → 0.555 s; 401-during-search-storm p90 0.148 s → 0.027 s, max 0.312 s → 0.041 s (scratch A/B, 153 MB corpus) | character-window content scan bounds per-call GIL holds in sidebar search; M8 definition and healthy range introduced with this PR |
| 2026-08-30 | #481 | M9 per-round spend recompute 0.402 s → 0.002 s steady state, 0.063 s with the 5.2 MB active file changed (200 rollout files, 33 in the 7-day window, 41 MB; identical spend results) | per-file spend-event memo keyed on (mtime_ns, size) on the codex usage provider; M9 definition and healthy range introduced with this PR |
| 2026-08-30 | #489 | M10 torn reads 38932/59366 → 0/35975 concurrent reads over 3000 save_metadata calls (collector verbatim; pre-fix code also 500ed one threads/list poll in the live server log) | thread metadata.json writes routed through the repo's atomic-write rule (atomic_write_text), mirroring the session-metadata path; M10 definition and healthy range introduced with this PR |
| 2026-08-30 | #492 | M11 GET /api/backlog + /api/backlog/history 500/500 → 200/200 `[]` on the unconfigured host (live-before with 5 backlog 500s in the 16 h server log; scratch TestClient A/B for the after) | unconfigured backlog reads return the empty state /repos already reports; PATCH keeps the loud raise; M11 definition and healthy range introduced with this PR |
| 2026-08-31 | #496 | M12 median 0.0071 s → 0.0006 s, max 0.0073 s → 0.0008 s (200 rollout files, 0.98 MB newest rollout; scrape results identical modulo fetched_at) | per-newest-rollout usage memo keyed on (mtime_ns, size) plus a 1 MiB tail-window read on the codex provider; M12 definition and healthy range introduced with this PR |
| 2026-08-31 | #502 | M7 warm median 1.609 s → 0.846 s collector-level, 1.758 s → 1.158 s HTTP-level (scratch A/B: base #496 vs branch, scratch CHARLIEBOT_HOME each; 200 rollout files 180 MB, 4 Claude homes 676 MB, opencode.db 10.5 GB; tallies byte-identical) | codex rollouts joined the persisted per-file tally cache (token_count records plus the root-session self-check pair) |
| 2026-08-31 | #509 | M13 median 0.0673 s → 0.0000 s, max 0.0818 s → 0.0001 s (6.7 MB worst worker log, 2177 projected events; projection identical including timestamps) | byte-offset incremental read with per-path cached projection for the workers-panel thread-events poll; M13 definition and healthy range introduced with this PR |
| 2026-08-31 | #510 | M5 live median 0.026 s→ (scratch A/B on the 154-thread worst session's copied metadata corpus) list_threads steady state 14.72 ms → 0.96 ms, max 17.45 ms → 1.22 ms; list output byte-identical | per-file (mtime_ns, size) memo plus an os.scandir/str-path scan for the workers-panel threads poll |
| 2026-08-31 | #512 | M14 loop-lag median 0.1504 s → 0.0060 s, max 0.1616 s → 0.0061 s (463 files in charlie-bot root..HEAD; diff payload identical) | git API subprocess calls moved off the event loop via asyncio.to_thread; M14 definition and healthy range introduced with this PR |
| 2026-08-31 | #518 | M15 torn reads 24521/44026 → 0/51495 concurrent reads over 3000 _write_cache_entry calls (collector verbatim, main-checkout before vs branch after, scratch state) | recap summary-cache writes routed through the repo's atomic-write rule (write_json_atomically), mirroring the session/thread metadata paths; M15 definition and healthy range introduced with this PR |
| 2026-08-31 | #525 | M16 torn reads 87232/128449 (PR base) and 48061/71863 (main checkout) → 0/51072 concurrent reads over 3000 _save_trigger calls (collector verbatim, scratch state) | trigger-file writes routed through the repo's atomic-write rule (atomic_write_text), mirroring the session/thread metadata and recap-cache paths; M16 definition and healthy range introduced with this PR |
| 2026-09-01 | #526 | M17 fork median 0.4350 s → 0.2126 s, max 0.4850 s → 0.2554 s (5519 parent events over a 36.3 MB archive+live corpus, scratch CHARLIEBOT_HOME A/B; reference bytes identical to the pre-fix output) | full-corpus forks copy raw parent event lines into parent_reference.jsonl instead of parse+reserialize; first M17 history row |
| 2026-09-01 | #530 | M6 median 0.0119 s → 0.0046 s, max 0.0123 s → 0.0051 s (scratch TestClient A/B on the 20534-event worst session's copied corpus; live-before median 0.014 s, max 0.022 s; usage payload identical) | whole-result usage-facts memo keyed on the cached events list's identity+length (LRU cap 8); load+scan moved off the event loop into the load's to_thread |
| 2026-09-01 | #533 | M7 live-before median 1.526-1.662 s → scratch-after warm median 0.443 s, max 1.158 s (scratch-server A/B, scratch CHARLIEBOT_HOME with empty cache, cold 10.49 s; collector-level warm 0.9105 s → 0.5261 s, warm re-scan 10.4 MB → 0 bytes; cached vs cacheless tallies byte-identical at a pinned db signature) | opencode db contribution joined the persisted tally cache, signatured on the main db file plus its WAL sidecar |
| 2026-09-01 | #535 | M18 241 → 0 poll fetches per simulated 10 hidden min (one expanded running worker + ext-usage strip; 1 bootstrap fetch excluded; 10 s visible re-check 4 fetches before and after) | workers-panel thread-detail poll and ext-usage strip timers registered through page-timers like every other timer; M18 definition and healthy range introduced with this PR |
| 2026-09-01 | #538 | M19 median 2.6548 s → 0.1151 s, max 2.8682 s → 0.1377 s (16.0 MB payload in 16 KB chunks, ~1 MB frames; framed line stream identical) | terminator search resumes at the proven-clean remainder cursor instead of re-scanning the whole remainder per chunk; M19 definition and healthy range introduced with this PR |
| 2026-09-01 | #542 | M20 repeat-divider extract median 0.2026 s → 0.0000 s, max 0.2429 s → 0.0000 s (5519 events over 36.3 MB corpus, scratch CHARLIEBOT_HOME A/B; extraction digest identical bb99828aa5b6; live-before recap HTTP median 0.305 s, max 0.538 s on the same session) | (session_id, divider end) LRU memo for extract_recap with a count-free explicit-divider hit path; M20 definition and healthy range introduced with this PR |
| 2026-09-01 | #545 | M8 median 0.2430 s → 0.0155 s, max 0.2504 s → 0.0176 s (scratch A/B, identical 147.6 MB / 33-session corpus, identical result sets; live-before median 0.235 s, max 0.259 s; remaining 15 ms is metadata loading — memoized repeats read zero corpus bytes) | per-chat-file proven-absent-needle LRU memo keyed on (mtime_ns, size); identical and superstring queries serve with one stat per file, appends re-read |
| 2026-09-01 | #549 | M21 steady-state sweep median 0.0353 s → 0.0068 s, max 0.0358 s → 0.0070 s (33 active sessions, live corpus; 33 → 0 deep probes per sweep; cold sweep unchanged at 33; probe entries identical) | stat-only probe-input signature (thread metadata/trigger/plans (mtime_ns, size) + name sets + 30-day rollover) narrows the 10th-poll self-heal sweep; force=1 keeps the full deep probe; M21 definition and healthy range introduced with this PR |
| 2026-09-01 | #554 | M22 180 → 0 ext_usage_unknown_limit_shape warnings per 60 steady-state transform rounds (collector verbatim, main-checkout before vs branch after; transform windows identical over the 60-round repeat; live-log corroboration 1536 lines in 20.47 h ≈ 75/h) | all eight emitters of the event routed through a (provider, account, slot, reason) warn-once guard — one alarm per unrecognized shape per process; M22 definition and healthy range introduced with this PR |
| 2026-09-01 | #560 | M23 8-page scroll steady-state median 0.0926 s → 0.0022 s, max 0.0952 s → 0.0024 s (collector verbatim, main-checkout before vs branch after; 8.2 MB across 2 weekly archive files, archive_offset 2799, scratch CHARLIEBOT_HOME A/B under load 4.49/4.58/4.61; page contents asserted identical) | per-archive-file parsed-events memo keyed on (mtime_ns, size) with a 16-file LRU, mirroring the live events cache; M23 definition and healthy range introduced with this PR |
| 2026-09-01 | #566 | M24 steady-state median 0.0164 s → 0.0006 s, max 0.0222 s → 0.0008 s (collector verbatim, main-checkout before vs branch after; 78-file worst trigger corpus, live state read-only; serialized list output over the top-5 trigger sessions identical) | per-trigger-file parsed-record memo keyed on (mtime_ns, size) with a name-set diff for deletions and a 32-session LRU; M24 definition and healthy range introduced with this PR |
| 2026-09-01 | #568 | M25 steady-state loop-lag median 0.0115 s → 0.0001 s (0.000083 s wall), max 0.0116 s → 0.0001 s (0.000091 s wall) (collector verbatim, main-checkout before vs branch after at load 0.36/0.55/0.62 and 0.64/0.60/0.63; live config corpus read-only; after run under the 5 ms ticker resolution) | scheduler _reload_config routed through the fingerprint-cached get_config instead of a per-tick load_config parse; M25 definition and healthy range introduced with this PR |
| 2026-09-01 | #572 | M26 projection advance median 46.69 ms → 0.19 ms, max 100.40 ms → 0.27 ms (collector verbatim, 8 single-event appends on the 20534-event worst live corpus, scratch CHARLIEBOT_HOME A/B, main checkout before at load 2.35/1.88/1.51 vs final branch head after at load 1.35/1.32/1.75; served-view digest identical e94c56635194) | append-incremental message projection: closed-prefix single feed plus cloned-aggregator open-region view, advanced copies swapped in atomically; M26 definition and healthy range introduced with this PR |
| 2026-09-01 | #576 | M27 steady-state tolerant read median 319.3 µs → 10.6 µs, max 355.6 µs → 41.5 µs (collector verbatim, main checkout before at load 1.28/1.17/1.15 vs final branch head after at load 0.91/0.97/1.02; worst plans corpus 15.4 KB / 12 plans, live state read-only; projected payload identical) | per-file (mtime_ns, size) memo with a 32-path LRU for read_plans_tolerant, mirroring the M24 trigger-list memo; OSError reads answered fresh every call per the sibling memo policy; M27 definition and healthy range introduced with this PR |
| 2026-09-02 | #581 | M28 parse_ndjson_tail median 50.20 ms → 17.34 ms, max 52.47 ms → 18.35 ms; count_ndjson_lines median 46.68 ms → 13.13 ms (collector verbatim, 36.3 MB / 5519-line worst live chat file, main checkout before vs final branch head after at load 0.54-0.71; tail events and line counts identical) | newline counting per 1 MiB chunk through a numpy SIMD compare replacing Python per-line iteration (~3.4 GB/s vs ~0.7 GB/s measured on this host), file-iteration count contract preserved; M28 definition and healthy range introduced with this PR |
| 2026-09-02 | #588 | M29 steady-state listing median 9.59 ms → 2.03 ms, max 11.64 ms → 2.30 ms (collector verbatim, 34 active sessions / 976 session dirs, live corpus read-only, main checkout before vs branch after back-to-back at load 0.84/0.90/0.87; full listing byte-identical) | os.scandir DirEntry names with d_type is_dir replacing Path.iterdir()+per-entry stat in the _load_session_metas preamble; M29 definition and healthy range introduced with this PR |
| 2026-09-02 | #592 | M7 scratch-server warm median 0.647 s → 0.326 s, max 0.667 s → 0.344 s, cold 20.9 s → 9.4 s at load 1.3-1.9; collector-level paired medians: quiet-db 0.62 s → 0.37 s, rescan-db 1.50-1.67 s → 1.02 s; live-before median 1.721 s measured against pre-#510/#533 live code (flagged); cacheless tally rows+notes digests pairwise identical | in-process aggregate memo serves the merged Claude/Codex partial keyed on the walk signature, and the opencode db scan projects its eight tally fields through json_extract; document save moves to the miss path |
| 2026-09-02 | #597 | M19 median 0.1072 s → 0.0247 s, max 0.1136 s → 0.0257 s (collector verbatim, main checkout before vs final branch head after back-to-back at load 0.80/0.99/1.07; framed line stream identical, 32 lines; LF-dense 1.4 MB / 64 KB-chunk production shape 0.0922 s → 0.0840 s) | chunk terminator search through cached-CR str.find passes instead of the alternation regex (re engine ~0.087 s per 16 MB measured vs memchr speed), piecewise fragment accumulation instead of concatenating the accumulated remainder per chunk |
| 2026-09-02 | #600 | M30 8-page live-half scroll steady-state median 0.0453 s → 0.0002 s, max 0.0500 s → 0.0002 s (scratch CHARLIEBOT_HOME A/B, main checkout before vs final branch head after back-to-back at load 0.82/0.59/0.74; 6.3 MB live file of archived session 3b91d606, archive_offset 1136, total 3760 events; collector verbatim on the branch 0.0004 s median) | per-physical-line parsed-events memo on the live file keyed on (mtime_ns, size), 4-file LRU, gated to archive_offset > 0 sessions (unarchived ones paginate via the M26 projection), mirroring the M23 archive memo; M30 definition and healthy range introduced with this PR |
| 2026-09-02 | #603 | M31 steady-state median 0.0377 s → 0.0016 s, max 0.0605 s → 0.0021 s (collector verbatim, 6.7 MB worst worker log, live state read-only, main checkout before at load 0.58/1.48/1.29 vs branch after at load 0.91/1.50/1.30; summary output identical on the worst log; sibling review-context delegation scan 159.4 → 13.4 / 60.9 / 148.2 ms medians by front/mid/tail match position, context identical) | read_events_summary collects the last-80 parseable events through 512 KiB from-the-end segments (parse_ndjson_tail_parseable) instead of full-parsing the log; extract_review_context streams to the thread's first task_delegated instead of full-parsing the chat log; M31 definition and healthy range introduced with this PR |
| 2026-09-02 | this PR | M32 steady-state median 3.88 ms → 0.57 ms, max 3.95 ms → 0.67 ms (collector verbatim, 68 entry files, live memory corpus read-only, main checkout before at load 0.92/0.71/0.69 vs branch after at load 0.47/0.62/0.66, back-to-back; assembled block sha256-identical c561298a159a) | load_store memoized on a stat-only (path, mtime_ns, size) signature over every parsed file, walked with os.scandir + string joins (Path.glob/relative_to allocation would dominate the stats); only successful loads memoize, a corrupt rewrite drops the stale hit; M32 definition and healthy range introduced with this PR |
| 2026-09-02 | this PR | M33 replay wall median 2.624 s → 0.589 s, max 2.829 s → 0.649 s (collector verbatim, 98.0 KB worst on-disk assistant draft sha1 6e0cb6e8f159, 502 deltas at 40 ms virtual cadence, 502 → 102 paints, main checkout before vs final branch head after back-to-back at load 0.22/0.43/0.87; final-frame parity true both arms) | stream-draft paints coalesced to a 200 ms leading+trailing cadence, hideStreaming cancels the pending trailing paint (every terminal path hides first: committed bubble, error, session swap); M33 definition and healthy range introduced with this PR |
