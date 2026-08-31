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
persisted per-file tally cache for the gigabyte-scale Claude logs and the hundred-megabyte
Codex rollouts; the first load after a server start (or after bulk log churn) is a full
scan, later loads re-scan only changed logs plus the megabyte-scale opencode source, so the
five-request median reads the warm steady state (~2 s at the seed host's 2.2 GB + 190 MB log
volume, measured before the Codex logs joined the cache — hence the provisional 3 s
ceiling). Evidence while the live server runs older code is a scratch-instance
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
