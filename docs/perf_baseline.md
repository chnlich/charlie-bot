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
| M30 live-half chat-event range rescan, steady state | M30 collector below | seconds per 8-page backwards scroll over the biggest archived session's live file; the append-round repeat (one appended line before each timed round, the page click during a streamed turn) | unchanged median < 0.005 s; append-round median < 0.005 s | — (introduced with its first history row) |
| M31 worker-finalize events-summary read, steady state | M31 collector below | seconds per `read_events_summary` call, worst on-disk worker log | median < 0.02 s | — (introduced with its first history row) |
| M32 memory-store assemble, steady state | M32 collector below | seconds per `assemble_master` call, live memory corpus | median < 0.005 s | — (introduced with its first history row) |
| M33 assistant-stream draft render, full-turn replay | M33 collector below | seconds per replay of the largest on-disk assistant draft, 200 B deltas at 40 ms virtual cadence | median < 1.0 s | — (introduced with its first history row) |
| M34 worker-events poll fetch at rendered count | M34 collector below | seconds + response bytes per events fetch, worst on-disk worker log | after=total median < 0.02 s; empty-tail body < 200 B | — (introduced with its first history row) |
| M35 chat message-page responses, steady state | M35 collector below | seconds per request, worst projection corpus | events page median < 0.03 s | — (introduced with its first history row) |
| M36 worker list poll payload and handler time, steady state | M36 collector below | seconds per list request + response body bytes, worst thread-metadata corpus; the conditional repeat (?etag=) of an unchanged poll | full median < 0.02 s; full body < 200 KB; conditional body 0 B (204) | — (introduced with its first history row) |
| M37 archived-session chat tail page, steady state | M37 collector below | seconds per `parse_ndjson_tail(200)` call, worst on-disk archived live file | median < 0.005 s | — (introduced with its first history row) |
| M38 session stream-broadcast fan-out, worst on-disk turn replay | M38 collector below | stream frames and json.dumps calls/seconds per turn replay, instant feed, one subscriber | dumps total < 0.02 s; final-frame parity true | — (introduced with its first history row) |
| M39 tui/status busy check, steady state | M39 collector below | seconds of loop lag + wall per per-session busy check, live ~/.claude/projects corpus (loop lag reads the 5 ms ticker floor like M14) | loop-lag median < 0.01 s; wall median < 0.001 s | — (introduced with its first history row) |
| M40 session-list filtered listing + group reduction, steady state | M40 collector below | seconds per `list_sessions(starred=True, …)` call and per group-name reduction, live session corpus | both medians < 0.005 s | — (introduced with its first history row) |
| M41 git diff/files repeat view, steady state | M41 collector below | seconds per repeat `diff_files` call over the charlie-bot root..HEAD range | median < 0.02 s | — (introduced with its first history row) |
| M42 scheduler tick, steady state | M42 collector below | seconds of loop lag per 60 s tick with no task due, live config + session corpus (loop lag reads the 5 ms ticker floor like M14) | median < 0.01 s | — (introduced with its first history row) |
| M43 git diff/file repeat expand, steady state | M43 collector below | seconds per repeat `diff_file` call over the heaviest file of the charlie-bot root..HEAD manifest | median < 0.02 s | — (introduced with its first history row) |
| M44 scheduled-list next-run resolution, steady state | M44 collector below | seconds per `GET /api/sessions/scheduled` request, live session + cron corpus | median < 0.004 s | — (introduced with its first history row) |
| M45 session-WS catchup replay event-loop lag, stale-cursor reconnect | M45 collector below | seconds of loop lag + wall per `_replay_aggregated_catchup` run, worst on-disk live chat corpus, cursor 50 events behind (loop lag reads the 5 ms ticker floor like M14) | loop-lag median < 0.05 s | — (introduced with its first history row) |
| M46 cron tasks list payload and handler time, steady state | M46 collector below | seconds per request + response body bytes, live cron corpus | median < 0.02 s; body < 20 KB | — (introduced with its first history row) |
| M47 claude declared-window warning stream, steady state | M47 collector below | warnings per 60 steady-state declared-window resolutions | 0 warnings after the first sighting per process | — (introduced with its first history row) |
| M48 search content-scan missing-file debug stream, steady state | M48 collector below | debug lines per 60 steady-state content scans of an active session whose live chat file is missing | 0 lines after the first sighting per (session, error) per process | — (introduced with its first history row) |
| M49 opencode part-unhandled debug stream, steady state | M49 collector below | debug lines per 60 steady-state `_translate_part` calls of one unhandled part type | 0 lines after the first sighting per part type per process | — (introduced with its first history row) |
| M50 ext-usage credentials read warning stream, steady state | M50 collector below | warnings per 60 steady-state `_read_credentials` calls of a tokenless file | 0 warnings after the first sighting per (event, path) per broken streak | — (introduced with its first history row) |
| M51 sidebar dirty-session deep probe, post-write | M51 collector below | seconds per post-write deep probe of the worst on-disk threads corpus (scratch copy, one atomic metadata rewrite per round) | median < 0.010 s | — (introduced with its first history row) |
| M52 chat-event append, per event | M52 collector below | seconds per `save_chat_event` append of one probe event, worst on-disk live-events corpus, scratch home | median < 0.0003 s | — (introduced with its first history row) |
| M53 config reload failure re-fire, broken steady state | M53 collector below | warnings + re-parses per 60 steady-state `get_config` calls of a persistently-broken config corpus | 0 warnings after the first sighting per (event, error) per process; 0 re-parses (one fingerprint stat set per call) | — (introduced with its first history row) |
| M54 stream-draft paint work, code-bearing draft, real highlight.js | M54 collector below | seconds of paint work per full-turn replay of the largest fence-bearing on-disk assistant draft, 200 B deltas at 40 ms virtual cadence, page-pinned marked + hljs 11.9.0 common builds | median < 0.2 s | — (introduced with its first history row) |
| M55 artifact compare-view serve, steady state | M55 collector below | seconds per repeat `?diff=` compare-view request over the worst on-disk artifact pair, plus the request's worst event-loop gap (the 5 ms ticker floor like M14) | repeat-view median < 0.010 s; loop-lag median < 0.010 s | — (introduced with its first history row) |
| M56 sidebar status poll, steady state | M56 collector below | seconds per `GET /api/sessions/status` request over the active-session id set | median < 0.0028 s | — (introduced with its first history row) |
| M57 plan-registry poll, steady state | M57 collector below | seconds per `GET /api/sessions/{id}/plans` request, worst on-disk plans corpus | median < 0.0030 s | — (introduced with its first history row) |

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
  ['thread-events-t1', {innerHTML: '', dataset: {}, parentElement: {querySelector: () => null, insertBefore() {}}}],
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
the directory record itself (~1 ms), itself memoized on the root's own
(mtime_ns, size) — the mtime moves exactly when a session entry is created or
removed — so a repeat listing with an unchanged root pays one stat (~5 µs,
signature taken before the scan so a racing create/delete re-scans next call).
The cost is a slice of every listing call
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
live_path = home / "sessions" / SID / "data" / "chat_events.jsonl"

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
scroll()
atimes = []
for i in range(8):
    with open(live_path, "a", encoding="utf-8") as f:  # the scratch copy, never the live home
        f.write(json.dumps({"id": f"m30-append-probe-{i}", "type": "assistant",
                            "message": {"content": [{"type": "text", "text": "probe"}]},
                            "timestamp": "2026-09-04T19:00:00Z"}) + "\n")
    t0 = time.perf_counter()
    scroll()
    atimes.append(time.perf_counter() - t0)
atimes.sort()
print(f"archive_offset {offset}, total {total}; 8-page live-half scroll steady-state "
      f"median {times[2]:.4f} s, max {times[-1]:.4f} s; append-round median {atimes[3]:.4f} s, "
      f"max {atimes[-1]:.4f} s over 8")
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

M34 — worker-events poll fetch at the client's rendered count. The 5 s
workers-panel events poll served the whole projected history every round —
860706 B and 2177 events for the worst on-disk log; the projection itself
is memoized (M13), but serialization, transfer, and the client's full
innerHTML rebuild stayed O(history) per poll. The endpoint's ``after=N``
returns only the events past the client's rendered raw count (a sound
prefix cut: ``_append_worker_events`` never rewrites an emitted row) and
answers ``reset`` + the full payload when the count runs ahead; the client
appends tails through a scratch paint plus ``insertAdjacentHTML``. The
steady state (the metric) is a poll with nothing new. The collector copies
the worst on-disk worker log into a scratch ``CHARLIEBOT_HOME`` (live home
read once, never written), warms the memo, and times five full fetches (the
pre-fix behavior, a byte-identical code path) and five ``after=total``
fetches, asserting the empty-tail shape; the pytest suite pins prefix+tail
parity. Evidence points the same collector at the branch checkout
(``CHECKOUT`` at the worktree root), the same shape as the M7 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from fastapi.testclient import TestClient
from src.api.deps import get_thread_manager
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig
from src.core.threads import ThreadManager

# Worst worker-events corpus: the largest events.jsonl on disk; live home
# read once for the copy, never written; the endpoint reads only the copy.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for p in root.glob("*/threads/*/data/events.jsonl"):
    n = p.stat().st_size
    if n > best_n:
        best, best_n = p, n
SID, TID = best.parts[-5], best.parts[-3]
home = Path(tempfile.mkdtemp(prefix="m34-events-home-", dir="/tmp"))
dst = home / "sessions" / SID / "threads" / TID / "data" / "events.jsonl"
dst.parent.mkdir(parents=True)
shutil.copy2(best, dst)

app = FastAPI()
app.include_router(threads_router, prefix="/api/threads")
cfg = CharlieBotConfig(charliebot_home=home)
app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
client = TestClient(app)
url = f"/api/threads/{SID}/threads/{TID}/events"

full = client.get(url)  # cold pass, as at first panel open after a server start; not timed
total = len(full.json())
full_times, full_bytes = [], 0
for _ in range(5):
    t0 = time.perf_counter()
    r = client.get(url)
    full_times.append(time.perf_counter() - t0)
    full_bytes = len(r.content)
full_times.sort()
inc_times, inc_bytes = [], 0
for _ in range(5):
    t0 = time.perf_counter()
    r = client.get(url, params={"after": total})
    inc_times.append(time.perf_counter() - t0)
    inc_bytes = len(r.content)
inc_times.sort()
env = r.json()
assert env["reset"] is False and env["total"] == total and env["events"] == []
print(f"{best_n / 1e6:.1f} MB log, {total} events; full fetch median {full_times[2]:.4f} s, "
      f"max {full_times[-1]:.4f} s ({full_bytes} B); after=total median {inc_times[2]:.4f} s, "
      f"max {inc_times[-1]:.4f} s ({inc_bytes} B)")
shutil.rmtree(home)
EOF
```

M35 — chat message-page responses (events/view/bootstrap), steady state. The chat
pagination endpoint (``GET /api/sessions/{id}/events``), the SPA-switch session view,
and the bootstrap payload return their message pages through FastAPI's default
response path, whose jsonable_encoder walk measures ~3x a plain json.dumps on the
211-message / 559 KB worst projection page (9 ms vs 3 ms) — the same gap the M34
envelope documented on mapped pydantic lists — while the memoized projection behind
the page serves O(page) (M26), so the encoder pass is the endpoint's dominant
server-side cost. The fixed handlers return a JSONResponse over the payload
directly; every field is already a plain parsed-JSON type or
``model_dump(mode="json")`` output, so the dumped body is byte-identical. The cost
is per page click / SPA switch, invisible to the standing HTTP probes, so the
collector snapshots the worst projection corpus (the session with the most live
chat events) into one shared scratch ``CHARLIEBOT_HOME`` (live home read once for
the copy, never written) and drives the three endpoints through TestClient in each
checkout's process: one cold pass per endpoint, as at first view after a server
start, then five timed requests, with digests read off the last timed response so
the view/bootstrap mark_read write-once cannot skew the cross-checkout comparison.
Evidence points the same collector at the before and after checkouts (``CHECKOUT``
at each root, shared ``M35_HOME`` snapshot), asserting byte-identical bodies, the
same shape as the M7 protocol. Snapshot once:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import shutil, tempfile
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
home = Path(tempfile.mkdtemp(prefix="m35-msg-page-home-", dir="/tmp"))
shutil.copytree(best, home / "sessions" / best.name)
print(f"export M35_HOME={home} M35_SID={best.name} M35_N={best_n}")
EOF
```

Then run per checkout (``eval`` the snapshot export first):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import hashlib, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from fastapi.testclient import TestClient
import src.api.deps as deps
from src.api.deps import get_session_manager, get_thread_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig, get_config
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

home = Path(os.environ["M35_HOME"])
SID = os.environ["M35_SID"]
BEFORE_N = int(os.environ["M35_N"])

# Scratch wiring: managers and config resolve to the snapshot; the view handler's
# direct get_trigger_manager() call is seeded with the scratch manager too.
cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
deps._trigger_manager = TriggerManager(cfg, mgr)
app = FastAPI()
app.include_router(sessions_router, prefix="/api/sessions")
app.dependency_overrides[get_session_manager] = lambda: mgr
app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
app.dependency_overrides[get_config] = lambda: cfg
client = TestClient(app)

def timed(url, params):
    client.get(url, params=params)  # cold pass, as at first view after a server start; not timed
    times = []
    body = None
    for _ in range(5):
        t0 = time.perf_counter()
        r = client.get(url, params=params)
        times.append(time.perf_counter() - t0)
        body = r.content
    times.sort()
    return times, body

ev_t, ev_b = timed(f"/api/sessions/{SID}/events", {"before": BEFORE_N, "limit": 200})
vw_t, vw_b = timed(f"/api/sessions/{SID}/view", None)
bt_t, bt_b = timed(f"/api/sessions/{SID}/bootstrap", None)
d = lambda b: hashlib.sha256(b).hexdigest()[:12]
print(f"events page median {ev_t[2] * 1000:.2f} ms, max {ev_t[-1] * 1000:.2f} ms ({len(ev_b)} B); "
      f"view median {vw_t[2] * 1000:.2f} ms, max {vw_t[-1] * 1000:.2f} ms ({len(vw_b)} B); "
      f"bootstrap median {bt_t[2] * 1000:.2f} ms, max {bt_t[-1] * 1000:.2f} ms ({len(bt_b)} B); "
      f"digests events {d(ev_b)} view {d(vw_b)} bootstrap {d(bt_b)}")
EOF
```

M36 — worker list poll payload and handler time, steady state. The 3 s
workers-panel poll (``GET /api/threads/{sid}/list``) serves an unchanged
session from the whole-body memo; a poll repeating the ETag it rendered via
``?etag=`` gets a bodyless 204 instead of the full rows, so the steady state
transfers zero body bytes and the client skips its JSON.parse. The conditional
rides a query param rather than If-None-Match because the browser's HTTP cache
fulfils a revalidation itself and fetch never surfaces the 304; no-store on
every answer keeps each poll a real request. The collector drives the endpoint
through TestClient over the session whose threads directory carries the most
metadata bytes (live state read-only), with the thread and trigger managers
built once as the server's dependency singletons are — per-request manager
instances would rebuild the M5/M24 memos on every call and drown the measured
path in a memo-cold scan the live server never pays. One cold pass, as at
first panel paint after a server start, then seven timed full requests
(first paint / changed rows; byte-identical code path before and after) and
seven timed conditional repeats. Evidence while the live server runs older
code points the same collector at the branch checkout (``CHECKOUT`` at the
worktree root), the same shape as the M7 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from fastapi.testclient import TestClient
from src.api.deps import get_thread_manager, get_trigger_manager
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

# Worst worker-list corpus: the session whose threads carry the most metadata
# bytes; the endpoint reads live state read-only. Managers are built once, as
# the server's dependency singletons are.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    t = d / "threads"
    if t.is_dir():
        n = sum((p / "metadata.json").stat().st_size for p in t.iterdir() if (p / "metadata.json").is_file())
        if n > best_n:
            best, best_n = d, n
SID = best.name

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
thread_mgr = ThreadManager(cfg)
trigger_mgr = TriggerManager(cfg, SessionManager(cfg))
app = FastAPI()
app.include_router(threads_router, prefix="/api/threads")
app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
app.dependency_overrides[get_trigger_manager] = lambda: trigger_mgr
client = TestClient(app)
url = f"/api/threads/{SID}/list"

r = client.get(url)  # cold pass, as at first panel paint after a server start; not timed
etag = r.headers["ETag"]
full_times, full_bytes = [], 0
for _ in range(7):
    t0 = time.perf_counter()
    r = client.get(url)
    full_times.append(time.perf_counter() - t0)
    full_bytes = len(r.content)
full_times.sort()
cond_times, cond_bytes = [], 0
for _ in range(7):
    t0 = time.perf_counter()
    r = client.get(url, params={"etag": etag})
    cond_times.append(time.perf_counter() - t0)
    cond_bytes = len(r.content)
cond_times.sort()
assert r.status_code == 204 and cond_bytes == 0, (r.status_code, cond_bytes)
rows = client.get(url).json()
trunc = sum(1 for row in rows if "description_full_len" in row)
print(f"{best_n / 1e3:.0f} KB thread metadata over {len(rows)} rows in session {SID} ({trunc} truncated); "
      f"full poll median {full_times[3] * 1000:.2f} ms, max {full_times[-1] * 1000:.2f} ms, body {full_bytes} B; "
      f"conditional poll median {cond_times[3] * 1000:.2f} ms, max {cond_times[-1] * 1000:.2f} ms, body {cond_bytes} B")
EOF
```

M37 — archived-session chat tail page, steady state. Sessions with
``archive_offset > 0`` serve the chat view and bootstrap from
``load_chat_events_tail`` → ``parse_ndjson_tail`` (unarchived sessions use the
M26 message projection), and the pre-fix reader paid a full-file line count
plus the 512 KiB tail-window parse on every SPA switch — repeat work against
an unchanged file. The fixed reader memoizes the whole page and,
independently, the line count on (mtime_ns, size): a repeat over an
unchanged file pays one stat and zero file bytes; an append re-reads the
count and window. Each signature is taken before its read, so an entry
recorded during a concurrent append keys the older signature and can never
be served for the newer bytes (chat files only append; archive rewrites
replace the whole file). The line-count memo is shared with
``count_ndjson_lines``, the ``get_chat_event_count_sync`` path behind the
recap default divider. The cost is a per-view latency no standing HTTP
probe isolates (the biggest live files belong to unarchived sessions, which
route through the projection — the heaviest tail-page corpus on disk is the
7.4 MB archived-session live file), so the collector times the function the
view awaits over the worst on-disk archived live file (read-only), from the
checkout under test: one cold pass, as at first view of the session, then
nine timed repeats, asserting the served page is repeat-call identical.
Evidence while the live server runs older code points the same collector at
the branch checkout (``CHECKOUT`` at the worktree root), the same shape as
the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.ndjson import parse_ndjson_tail

# Worst tail-page corpus: the archived session whose LIVE chat file carries
# the most bytes; view/bootstrap tail pages count and parse it per call.
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
            best, best_n = live, n
print(f"worst archived live tail corpus: session {best.parts[-3]}, {best_n / 1e6:.1f} MB")

first = parse_ndjson_tail(best, 200)  # cold pass, as at first view of the session; not timed
times = []
result = None
for _ in range(9):
    t0 = time.perf_counter()
    result = parse_ndjson_tail(best, 200)
    times.append(time.perf_counter() - t0)
times.sort()
assert result == first, "served tail page differs between repeat calls"
events, total, has_more = result
print(f"{total} lines, tail events {len(events)}; steady-state parse_ndjson_tail(200) "
      f"median {times[4] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms")
EOF
```

M38 — session stream-broadcast fan-out, worst on-disk turn replay. Every stream part
feeds ``persist_and_broadcast``, whose aggregator emits a ``stream`` preview carrying the
whole accumulated draft; the pre-fix fan-out serialized that draft per delta per
subscriber (O(deltas × draft) of event-loop json.dumps: 78 ms / 174 frames on the worst
on-disk turn), and the fixed StreamingManager coalesces previews per channel (leading +
200 ms trailing windows at the client's showStreaming paint cadence, pending dropped only
on the preview-hiding types) and serializes once per fan-out. The collector resolves the
worst stream turn on disk (events between bare user events whose stream deltas carry the
most preview bytes; only files ≥ 2 MB contend), replays it instant-feed through the real
``MessageAggregator`` and ``StreamingManager`` into a stub socket mirroring starlette's
send_json (json.dumps wrapped process-wide so both arms count), stopping before
``master_done`` so both arms must deliver the final preview. Evidence points the
collector at the before and after checkouts (``CHECKOUT`` at each root):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.message_aggregator import MessageAggregator
from src.core.streaming import StreamingManager

class Probe:
  def __init__(self):
    self.frames = []
  async def send_json(self, data):
    self.frames.append(data)
    json.dumps(data, separators=(",", ":"), ensure_ascii=False)  # starlette send_json's cost
  async def send_text(self, text):
    self.frames.append(json.loads(text))

stats = {"calls": 0, "time": 0.0}
real_dumps = json.dumps
def timed_dumps(*a, **kw):
  t0 = time.perf_counter()
  out = real_dumps(*a, **kw)
  stats["calls"] += 1
  stats["time"] += time.perf_counter() - t0
  return out

def worst_stream_turn():
  # Turn: events between bare user events; score: summed stream-preview bytes
  # (the serialization driver); only files >= 2 MB can hold a contender.
  best = (-1, None, None)
  for p in Path.home().glob(".charliebot/sessions/*/data/chat_events.jsonl"):
    if p.stat().st_size < 2e6:
      continue
    events = []
    with open(p, errors="replace") as f:
      for line in f:
        try: events.append(json.loads(line))
        except json.JSONDecodeError: pass
    turns, cur = [], []
    for ev in events:
      if ev.get("type") == "user" and "message" not in ev:
        if cur:
          turns.append(cur)
        cur = [ev]
      else:
        cur.append(ev)
    if cur:
      turns.append(cur)
    for turn in turns:
      agg, score = MessageAggregator(), 0
      for i, ev in enumerate(turn):
        for d in agg.feed_indexed([(i, ev)]):
          if d["type"] == "stream":
            score += len(d["message"].get("content", "")) + len(d["message"].get("thinking", ""))
      if score > best[0]:
        best = (score, turn, p.parts[-3])
  return best[1], best[2]

async def main():
  turn, sid = worst_stream_turn()
  probe = Probe()
  manager = StreamingManager()
  await manager.subscribe("m38", probe)
  agg = MessageAggregator()
  final = None
  json.dumps = timed_dumps
  t0 = time.perf_counter()
  for i, ev in enumerate(turn):
    if ev.get("type") == "master_done":
      break  # stop before the commit; both arms must deliver the final preview first
    for d in agg.feed_indexed([(i, ev)]):
      if d["type"] == "stream":
        final = d
      await manager.broadcast("m38", d)
    if ev.get("type") not in ("assistant", "user", "scheduled_trigger"):
      await manager.broadcast("m38", ev)
  wall = time.perf_counter() - t0
  await asyncio.sleep(0.5)  # settle: lets the fixed arm's trailing flush land
  json.dumps = real_dumps
  streams = [f for f in probe.frames if f.get("type") == "stream"]
  parity = bool(streams) and final is not None and streams[-1] == final
  print(f"session {sid}; turn replay {wall:.2f} s, {len(streams)} stream frames, "
        f"{stats['calls']} json.dumps calls {stats['time'] * 1000:.0f} ms; final-frame parity {parity}")

asyncio.run(main())
EOF
```

M39 — tui/status busy check, steady state. The 3 s tui-status poll
(`fetchTuiStatus`, per visible tab) runs `_claude_jsonl_busy` per running tui
session, whose pre-fix form globbed all of `~/.claude/projects` inline on the
event loop on every call — ~15 ms of loop stall per check at this host's
942-dir projects corpus, delaying every concurrent request and WebSocket. The
fixed form memoizes the transcript path per session id (a hit is stable for
the session's life; a miss re-globs at a 30 s TTL) and awaits the check in a
thread. The collector drives the endpoint's fixed call shape
(`asyncio.to_thread` of `_claude_jsonl_busy`) with a concurrent 5 ms ticker
over a synthetic never-present session id — the glob cost is corpus-shaped,
identical for a real hit — from the checkout under test: one cold pass, as at
a server start with an empty memo, then nine timed steady-state checks (all
inside the miss TTL, so a glob would fire on every call were the memo absent).
The pre-fix number is the same command with `_claude_jsonl_busy(SID)` called
inline (the endpoint's pre-fix call shape) against an unmemoized import.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.agents.backends.tui import _claude_jsonl_busy

# Synthetic never-present id; the glob cost is corpus-shaped (~/.claude/projects
# dir count), identical for a real hit and this miss-shaped stand-in.
SID = "00000000-0000-0000-0000-000000000000"

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
    busy = await asyncio.to_thread(_claude_jsonl_busy, SID)  # the endpoint's call shape
    wall = time.perf_counter() - t0
    stop = True
    await t
    return busy, (max(gaps) if gaps else wall), wall

async def main():
    await run_once()  # cold pass, as at a server start with an empty memo; not timed
    results = []
    for _ in range(9):
        results.append(await run_once())
    lags = sorted(r[1] for r in results)
    walls = sorted(r[2] for r in results)
    n_dirs = sum(1 for _ in (Path.home() / ".claude" / "projects").iterdir())
    print(f"{n_dirs} project dirs; busy={results[0][0]}; "
          f"loop-lag median {lags[4]*1000:.2f} ms, max {lags[-1]*1000:.2f} ms; "
          f"busy-check wall median {walls[4]*1000:.2f} ms, max {walls[-1]*1000:.2f} ms")

asyncio.run(main())
EOF
```

M40 — session-list filtered listing and group-name reduction, steady state. The
sessions page's load fetches hit `GET /api/sessions/starred` and
`GET /api/sessions/groups`; the pre-fix `list_sessions` copied and
thinking-stamped every cached meta (~1012 rows at measurement) before the
starred/scheduled filters ran (the starred list keeps ~10), and the groups
handler plus the autonamer's group list paid the same full copy pass to reduce
the corpus to ~25 names — several ms of on-loop pydantic work per request.
The fixed `list_sessions` runs the starred/scheduled filters against the shared
cached metas (read-only) so only surviving rows pay the model_copy and thinking
stamp, and the group-name readers go through `list_group_names`, a read-only
reduction of the cached metas with no copies at all. The per-request enrich and
enrichment-free paths behind the standing status poll are unchanged. The cost
is a page-load latency no standing row covers, so the collector times the two
handler-level manager calls over the live session corpus (read-only), from the
checkout under test: one cold pass, as at a server start with an empty metadata
cache, then nine timed pairs. The pre-fix numbers used in the landing PR's
evidence are the same verbatim command with the pre-fix groups handler body
(`sorted({s.group for s in await mgr.list_sessions() if s.group})`) in place of
the `list_group_names()` call, the same shape as the M21 protocol. Evidence
while the live server runs older code points the same collector at the branch
checkout (`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

async def main():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    await mgr.list_group_names()  # cold pass, as at a server start; not timed
    stimes, gtimes, n_star, n_groups = [], [], 0, 0
    for _ in range(9):
        t0 = time.perf_counter()
        starred = await mgr.list_sessions(starred=True, include_running_status=True, include_pending_trigger_status=True)
        stimes.append(time.perf_counter() - t0)
        t0 = time.perf_counter()
        groups = await mgr.list_group_names()
        gtimes.append(time.perf_counter() - t0)
        n_star, n_groups = len(starred), len(groups)
    stimes.sort()
    gtimes.sort()
    print(f"starred list: {n_star} rows, median {stimes[4]*1000:.2f} ms, max {stimes[-1]*1000:.2f} ms; "
          f"group-name reduction: {n_groups} groups, median {gtimes[4]*1000:.2f} ms, max {gtimes[-1]*1000:.2f} ms")

asyncio.run(main())
EOF
```

M41 — git diff/files repeat view, steady state. The /diff viewer fetches
``GET /api/git/diff/files`` on every page open and on every refresh of a review
loop's pinned /diff link; each fetch ran one ``rev-parse`` and two sequential
``git diff`` subprocesses over the range (~0.18 s on the root..HEAD worst
corpus) to recompute an immutable result: a diff between two commits never
changes (SHAs are content-addressed). The fixed handler resolves both refs in
one rev-parse, runs the two manifest diffs concurrently on a miss, and serves
a repeat view of the same (repo, base_sha, head_sha, mode, .gitattributes
signature) from the manifest memo with zero diff subprocesses — the
attributes signature keys git's worktree diff drivers, which feed
``--numstat``'s counts. The cost is per page view, invisible to the standing
HTTP probes, so the collector drives ``diff_files`` over the charlie-bot
checkout's full history (root commit .. HEAD — the largest range this host's
workspace carries; read-only, with a scratch ``CHARLIEBOT_HOME``), from the
checkout under test: the first view, as at page open with a cold memo, then
seven timed repeats. Evidence while the live server runs older code points the
same collector at the branch checkout (``CHECKOUT`` at the worktree root), the
same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.api.git import diff_files

REPO = Path("/home/chaoli/workspace/charlie-bot")
BASE = subprocess.run(["git", "rev-list", "--max-parents=0", "HEAD"],
                      cwd=REPO, capture_output=True, text=True, check=True).stdout.splitlines()[0]
cfg = CharlieBotConfig(charliebot_home=Path(tempfile.mkdtemp(prefix="m41-home-")),
                       workspace_dirs=["/home/chaoli/workspace"])

async def main():
    t0 = time.perf_counter()
    result = await diff_files(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot", cfg=cfg)  # first view, as at page open; not a repeat
    cold = time.perf_counter() - t0
    times = []
    for _ in range(7):
        t0 = time.perf_counter()
        result = await diff_files(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot", cfg=cfg)
        times.append(time.perf_counter() - t0)
    times.sort()
    print(f"{result['total_files']} files in root..HEAD diff; first view {cold:.4f} s; "
          f"repeat-view median {times[3]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M42 — scheduler tick session scan, steady state. Every 60 s scheduler tick rebuilds the
per-task session cache from the session corpus; the pre-fix form copied and thinking-stamped
every cached meta (~1012 rows of on-loop pydantic work at measurement) only to read each
row's `scheduled_task` field, while the fixed form passes the M40 `scheduled=True` pre-copy
filter and copies just the surviving rows. The tick is background work invisible to HTTP
probes, so the collector drives `Scheduler._tick` over the live config and session
corpora with a concurrent 5 ms ticker, from the checkout under test: one cold pass, as at
a server start with an empty metadata cache, then five timed ticks. The steady state is
read-only: every enabled task keeps a matching-backend session and an unchanged cron (a
due fire is stubbed so no task spawns and no scheduler bookkeeping is written). Evidence
while the live server runs older code points the same collector at the branch checkout
(`CHECKOUT` at the worktree root), the same shape as the M25 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, sys, time
from unittest.mock import AsyncMock
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import get_config, get_scheduled_tasks
from src.core.sessions import SessionManager
from src.core.scheduler import Scheduler

async def main():
    cfg = get_config()
    sched = Scheduler(cfg, SessionManager(cfg))
    sched._execute_task = AsyncMock()  # fire stub: a due fire records without spawning
    await sched._tick()  # cold pass, as at a server start; not timed
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
        await sched._tick()
        stop = True
        await t
        worst.append(max(gaps) if gaps else 0.0)
    worst.sort()
    n_enabled = sum(1 for t in get_scheduled_tasks() if t.enabled)
    print(f"{n_enabled} enabled tasks; steady-state scheduler tick loop-lag median {worst[2]:.4f} s, "
          f"max {worst[-1]:.4f} s; fire stub awaited {sched._execute_task.await_count}x over 6 ticks")

asyncio.run(main())
EOF
```

M43 — git diff/file repeat expand, steady state. The /diff viewer fetches
``GET /api/git/diff/file`` on every file expand, and a collapse drops the
rendered body, so every re-expand, second tab, or refresh re-runs one
``git diff`` subprocess over the range (~44 ms on the heaviest file of the
charlie-bot root..HEAD manifest) to recompute an immutable result: a per-file
diff between two commits never changes (SHAs are content-addressed). The fixed
handler memoizes the body on the M41 manifest key plus the pathspec — repo,
resolved base/head SHAs, mode, .gitattributes signature (diff drivers feed the
emitted hunks), (old_path, path) — and builds the miss path's range spec from
the resolved SHAs, so a ref moving mid-request can never key one pair's body
under another; a repeat pays only the ref resolution. The cost is per file
expand, invisible to the standing HTTP probes, so the collector drives
``diff_file`` over the heaviest file of the charlie-bot checkout's root..HEAD
manifest (read-only, scratch ``CHARLIEBOT_HOME``), from the checkout under
test: the first expand, as at page open with a cold memo, then seven timed
repeats, asserting bodies repeat-identical. Evidence while the live server
runs older code points the same collector at the branch checkout (``CHECKOUT``
at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, os, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.api.git import diff_files, diff_file

REPO = Path("/home/chaoli/workspace/charlie-bot")
BASE = subprocess.run(["git", "rev-list", "--max-parents=0", "HEAD"],
                      cwd=REPO, capture_output=True, text=True, check=True).stdout.splitlines()[0]
cfg = CharlieBotConfig(charliebot_home=Path(tempfile.mkdtemp(prefix="m43-home-")),
                       workspace_dirs=["/home/chaoli/workspace"])

async def main():
    manifest = await diff_files(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot", cfg=cfg)
    row = max(manifest["files"], key=lambda f: f["additions"] + f["deletions"])
    path, old_path = row["path"], row.get("old_path")
    t0 = time.perf_counter()
    first = await diff_file(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot",
                            path=path, old_path=old_path, force=False, cfg=cfg)
    cold = time.perf_counter() - t0
    times = []
    repeat = None
    for _ in range(7):
        t0 = time.perf_counter()
        repeat = await diff_file(repo=str(REPO), base=BASE, head="HEAD", mode="three-dot",
                                 path=path, old_path=old_path, force=False, cfg=cfg)
        times.append(time.perf_counter() - t0)
    times.sort()
    assert repeat == first, "served file diff differs between repeat calls"
    print(f"heaviest manifest file {path} (+{row['additions']}/-{row['deletions']}), "
          f"size_bytes {first['size_bytes']}; first view {cold:.4f} s; "
          f"repeat-view median {times[3]:.4f} s, max {times[-1]:.4f} s")

asyncio.run(main())
EOF
```

M44 — scheduled-list next-run resolution, steady state. Every grouped sidebar
render pairs ``GET /api/sessions/scheduled`` with ``GET /api/cron/tasks`` (the
project-manager refresh fires on every list render), and the handler resolved
each scheduled row's next fire with one ``croniter(...).get_next`` expand per
row per request (~248 µs each measured, ~3 ms at the 12-task live corpus) — a
pure function of (cron, timezone, now) whose answer stays valid until the fire
time it names, so every repeat request inside that window recomputed an
identical string. The fixed handler serves rows from a memo keyed on (cron,
timezone), entries valid until their named fire time passes; a fire that went
by recomputes on the next request. The cost is a sidebar-render latency
invisible to the standing HTTP probes, so the collector drives the endpoint
through TestClient over the live session + cron corpora (read-only), with
managers built once as the server's dependency singletons are (per-request
instances would rebuild the M5/M24 memos on every call, drowning the measured
path in memo-cold scans the live server never pays): one cold pass, as at
first scheduled-tab open after a server start, then nine timed requests,
asserting the body is repeat-identical. Evidence while the live server runs
older code points the same collector at the branch checkout (``CHECKOUT`` at
the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio
import hashlib
import os
import sys
import time

sys.path.insert(0, os.environ["CHECKOUT"])
from pathlib import Path

from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_config, get_session_manager, get_thread_manager, get_trigger_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager


async def main():
  # Scratch wiring against the live home: managers built once, as the server's
  # dependency singletons are; read-only over the live session + cron corpus.
  import src.api.deps as deps
  cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
  mgr = SessionManager(cfg)
  deps._trigger_manager = TriggerManager(cfg, mgr)
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: mgr
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  app.dependency_overrides[get_trigger_manager] = lambda: deps._trigger_manager
  app.dependency_overrides[get_config] = lambda: cfg
  client = TestClient(app)

  url = "/api/sessions/scheduled"
  r = client.get(url)  # cold pass, as at first scheduled-tab open after a server start; not timed
  assert r.status_code == 200, (r.status_code, r.text[:200])
  times = []
  bodies = set()
  for _ in range(9):
    t0 = time.perf_counter()
    r = client.get(url)
    times.append(time.perf_counter() - t0)
    bodies.add(r.content)
  times.sort()
  # A body changing between repeats is live churn, not determinism: re-measure
  # rather than compare noise across arms.
  if len(bodies) != 1:
    print("live churn during measurement; re-run")
    raise SystemExit(1)
  digest = hashlib.sha256(r.content).hexdigest()[:12]
  rows = len(r.json())
  print(f"{rows} scheduled rows, body {len(r.content)} B, digest {digest}; "
        f"steady-state /scheduled median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms")


asyncio.run(main())
EOF
```

M45 — session-WS catchup replay event-loop lag, stale-cursor reconnect. When a session
WebSocket (re)connects behind the live event count (a mid-turn reconnect after a network
flap), `_send_session_catchup` replays the events past the cursor through
`_replay_aggregated_catchup`, which must feed the FULL event list to rebuild aggregator
state (a run interval that opened before the cursor drives the deltas after it). The
pre-fix form ran that feed loop inline on the event loop — 22 ms measured 2026-09-04
on the 20534-event worst live corpus — freezing every concurrent request and WebSocket
for the walk's wall time; the fixed form builds the frame list in a thread through
`_catchup_frames` and sends it in order, so the loop sees only the sends (identical
wire bytes, identical stop-at-first-failure count).
The cursor==total fast-skip never enters the replay, which the past day's 85 live
`session_ws_catchup_sent` log lines confirm (all sent=0), so this is a cold-path
insurance metric, invisible to the standing probes; the collector resolves the session
whose live chat file carries the most events, copies only that session into a scratch
`CHARLIEBOT_HOME` under /tmp (metadata.json and data/ only; live home read once for
the copy, never written), warms the events cache as the server's to_thread load does,
and replays at cursor = total − 50 (a reconnect 50 events behind) through a stub
socket with a concurrent 5 ms ticker, reporting the replay's worst ticker gap plus
wall — one cold pass, as at the first stale-cursor reconnect after a server start,
then five timed replays, with a frame-list digest pinning cross-checkout parity.
Evidence while the live server runs older code points the same collector at the
branch checkout (`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager
from server import _replay_aggregated_catchup

# Worst replay corpus: the session whose LIVE chat file carries the most
# events; a stale-cursor replay feeds every event before the cursor.
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

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m45-catchup-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")
cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)

class Stub:
    def __init__(self):
        self.frames = []
    async def send_json(self, data):
        self.frames.append(data)

async def main():
    events = await asyncio.to_thread(mgr.load_chat_events_sync, SID)
    total = len(events)
    cursor = total - 50  # stale-cursor reconnect shape: the client is 50 events behind
    stub = Stub()
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
        sent = await _replay_aggregated_catchup(stub, events, cursor, SID, event_index_offset=0)
        wall = time.perf_counter() - t0
        stop = True
        await t
        return sent, (max(gaps) if gaps else wall), wall
    await run_once()  # cold pass, as at the first stale-cursor reconnect after a server start; not timed
    results = []
    for _ in range(5):
        results.append(await run_once())
    walls = sorted(r[2] for r in results)
    gaps = sorted(r[1] for r in results)
    digest = hashlib.sha256(json.dumps(stub.frames, sort_keys=True, default=str).encode()).hexdigest()[:12]
    print(f"{total} events, cursor {cursor}, {results[0][0]} frames replayed, digest {digest}; "
          f"replay wall median {walls[2]:.4f} s, max {walls[-1]:.4f} s; "
          f"loop-lag median {gaps[2]:.4f} s, max {gaps[-1]:.4f} s")

asyncio.run(main())
shutil.rmtree(home)
EOF
```

M46 — cron tasks list payload and handler time, steady state. Every grouped
sidebar render pairs ``GET /api/cron/tasks`` with ``GET /api/sessions/scheduled``
(the project-manager refresh fires on every list render), and the pre-fix route
shipped every task's resolved prompt body — ~90 KB of the 96 KB live response,
content only the in-process scheduler/master reads (the UI edits
``prompt_file``) — plus the client pays a same-size JSON.parse per render. The
fixed dump excludes ``prompt``, mirroring the POST/PUT responses which never
carried it. The cost rides the sidebar render path, so the collector drives the
endpoint through TestClient over the live cron corpus (read-only:
``get_scheduled_tasks`` serves the fingerprint-cached snapshot; nothing is
written), one cold pass, as at first sidebar render after a server start, then
nine timed requests, re-running rather than comparing noise when the live
config drifts mid-measurement. Evidence while the live server runs older code
points the same collector at the branch checkout (``CHECKOUT`` at the worktree
root), the same shape as the M36 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, time
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from fastapi.testclient import TestClient
from src.api.cron import router as cron_router

# Live cron corpus read-only: get_scheduled_tasks resolves the process config's
# fingerprint-cached snapshot; nothing here writes.
app = FastAPI()
app.include_router(cron_router, prefix="/api/cron")
client = TestClient(app)
url = "/api/cron/tasks"

r = client.get(url)  # cold pass, as at first sidebar render after a server start; not timed
assert r.status_code == 200, (r.status_code, r.text[:200])
times = []
bodies = set()
for _ in range(9):
    t0 = time.perf_counter()
    r = client.get(url)
    times.append(time.perf_counter() - t0)
    bodies.add(r.content)
times.sort()
if len(bodies) != 1:
    raise SystemExit("live churn during measurement; re-run")
rows = r.json()
prompt_bytes = sum(len(row.get("prompt") or "") for row in rows)
step_prompt_bytes = sum(len(s.get("prompt") or "") for row in rows for s in (row.get("steps") or []))
print(f"{len(rows)} task rows, body {len(r.content)} B, prompt bytes {prompt_bytes}, step prompt bytes {step_prompt_bytes}; "
      f"steady-state GET /api/cron/tasks median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms")
EOF
```

M47 — claude declared-window warning stream, steady state. Every claude-session
usage resolution re-derives the headless declared window from the environment
(`_resolve_claude_tier` → `headless_claude_declared_window`), and while a
forwarded-but-unmodelled override is exported (this host sets
`CLAUDE_CODE_MAX_CONTEXT_TOKENS=400000`) each resolution re-fires the same
degradation warning — 62 lines in the 7.89 h live server log sampled 2026-09-04
(~8/h) — while one sighting per process carries the whole signal: the
environment the warning reports cannot change between resolutions. The cost is
background log volume invisible to HTTP probes, so the collector drives the
resolution directly with the override exported: one first-sighting call, as at
a process start, then 60 steady-state repeat calls, counting the event. A
warning in the repeat window is a re-fired alarm; the count is the metric.
Evidence while the live server runs older code points the same collector at the
branch checkout (`sys.path.insert` at the worktree root), the same shape as the
M22 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
os.environ["CLAUDE_CODE_MAX_CONTEXT_TOKENS"] = "400000"
from src.agents.backends import claude_code as claude_code_mod

warns = []
orig = claude_code_mod.log.warning
claude_code_mod.log.warning = lambda event, **kw: warns.append({"event": event, **kw})
try:
    claude_code_mod.headless_claude_declared_window()  # first sighting, as at a process start; not counted
    warns.clear()
    for _ in range(60):  # steady-state repeat resolutions of the usage path
        claude_code_mod.headless_claude_declared_window()
finally:
    claude_code_mod.log.warning = orig
n = sum(1 for w in warns if w["event"] == "claude_declared_window_degraded")
print(f"60 steady-state declared-window resolutions; claude_declared_window_degraded warnings: {n}")
EOF
```

M48 — search content-scan missing-file debug stream, steady state. Every
sidebar search (`GET /api/sessions/search?q=…`) content-scans the active
sessions whose names miss the query, and the scan's stat of a session whose
live chat file cannot be read (a fresh session's data/ stays empty until its
first event lands) logs `search_read_failed` on every request — 30 lines in
the 11.92 h live server log sampled 2026-09-04, all naming the one such
session — while one sighting per (session, error) carries the whole signal:
a repeat round that sees the same failure re-fires a fired alarm. The cost
is background log volume invisible to HTTP probes, so the collector creates
one fresh session in a scratch `CHARLIEBOT_HOME` and drives `search_sessions`
with an absent needle: one first-sighting scan, as at a process start, then
60 steady-state repeat scans, counting the event. A line in the repeat
window is a re-fired alarm; the count is the metric. Evidence while the live
server runs older code points the same collector at the branch checkout
(`sys.path.insert` at the worktree root), the same shape as the M22
protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, sys, tempfile
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.core import sessions as sessions_mod
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager

# Corpus shape: one fresh active session whose data/ holds no live chat file
# (a scheduled-session creation carries no events until its first turn).
work = Path(tempfile.mkdtemp(prefix="m48-search-scan-"))
cfg = CharlieBotConfig(charliebot_home=work / "home")
mgr = SessionManager(cfg)
asyncio.run(mgr.create_session(CreateSessionRequest(name="M48")))

events = []
orig = sessions_mod.log.debug
sessions_mod.log.debug = lambda event, **kw: events.append(event)
try:
    asyncio.run(mgr.search_sessions("zzq48neverpresent"))  # first sighting, as at a process start; not counted
    events.clear()
    for _ in range(60):  # steady-state repeat content scans of the sidebar search
        asyncio.run(mgr.search_sessions("zzq48neverpresent"))
finally:
    sessions_mod.log.debug = orig
n = sum(1 for event in events if event == "search_read_failed")
print(f"60 steady-state content scans of a no-live-file session; search_read_failed debug lines: {n}")
EOF
```

M49 — opencode part-unhandled debug stream, steady state. Every opencode SSE
`message.part.updated` frame routes through `_translate_part`, and a part type
the translator does not map logs `opencode_part_unhandled` per part — 152 lines
in the 12.88 h live server log sampled 2026-09-04 (~12/h), every one of them
`type=patch`, so a stream of unhandled parts re-fires the same line forever
while one sighting per part type carries the whole signal: the set of mapped
types is code, fixed for the process. The cost is background log volume
invisible to HTTP probes, so the collector drives `_translate_part` on a
synthetic unhandled part: one first-sighting call, as at a process start, then
60 steady-state repeat calls, counting the event. A line in the repeat window
is a re-fired alarm; the count is the metric. Evidence while the live server
runs older code points the same collector at the branch checkout
(`sys.path.insert` at the worktree root), the same shape as the M22 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import sys
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.agents.backends import opencode as opencode_mod

backend = opencode_mod.OpenCodeBackend()
part = {"id": "m49-p1", "messageID": "m49-m1", "type": "patch"}

events = []
orig = opencode_mod.log.debug
opencode_mod.log.debug = lambda event, **kw: events.append(event)
try:
    backend._translate_part(part)  # first sighting, as at a process start; not counted
    events.clear()
    for _ in range(60):  # steady-state repeat parts of the same unhandled type
        backend._translate_part(part)
finally:
    opencode_mod.log.debug = orig
n = sum(1 for event in events if event == "opencode_part_unhandled")
print(f"60 steady-state unhandled patch parts; opencode_part_unhandled debug lines: {n}")
EOF
```

M50 — ext-usage credentials read warning stream, steady state. Every
claude-account poll round re-reads the account's credentials file
(`ClaudeUsageProvider.fetch` → `_read_credentials`), and while the file is
missing or carries no access token each round re-fires the same warning —
135 lines in the 13.89 h live server log sampled 2026-09-04 (~10/h), every
one of them `ext_usage_no_access_token` naming the same path — while one
sighting per (event, path) per broken streak carries the whole signal: the
file is re-read every round, so a recovery (a read that returns a token)
re-arms the path and a later relapse is a new onset, earning one new line.
The cost is background log volume invisible to HTTP probes, so the collector
drives `_read_credentials` over a synthetic tokenless credentials file: one
first-sighting read, as at a process start, then 60 steady-state repeat
reads, counting the event. A warning in the repeat window is a re-fired
alarm; the count is the metric. Evidence while the live server runs older
code points the same collector at the branch checkout (`sys.path.insert` at
the worktree root), the same shape as the M22 protocol:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import json, sys, tempfile
from pathlib import Path
sys.path.insert(0, "/home/chaoli/workspace/charlie-bot")
from src.api import ext_usage as ext_usage_mod

work = Path(tempfile.mkdtemp(prefix="m50-cred-read-"))
creds = work / ".credentials.json"
creds.write_text(json.dumps({"claudeAiOauth": {"accessToken": "", "refreshToken": "r"}}))

warns = []
orig = ext_usage_mod.log.warning
ext_usage_mod.log.warning = lambda event, **kw: warns.append(event)
try:
    ext_usage_mod._read_credentials(creds)  # first sighting, as at a process start; not counted
    warns.clear()
    for _ in range(60):  # steady-state repeat reads of the poller's fetch rounds
        ext_usage_mod._read_credentials(creds)
finally:
    ext_usage_mod.log.warning = orig
n = sum(1 for event in warns if event == "ext_usage_no_access_token")
print(f"60 steady-state credential reads of a tokenless file; ext_usage_no_access_token warnings: {n}")
EOF
```

M51 — sidebar dirty-session deep probe, post-write. The 3 s status poll deep-probes a
session whenever its stat-only probe-input signature moved — which is every poll that
follows any write to the session (session/thread metadata writes mark it dirty), i.e.
continuously during an active turn. The deep probe's read path re-enters
`iter_recent_thread_metas`, which read and re-parsed every in-window (30-day) thread
metadata file per probe. The collector copies the session whose threads dir carries the
most metadata files (the M5 resolution rule) into a scratch `CHARLIEBOT_HOME` under /tmp
(live home read once for the copy, never written), warms the probe as a server start
does, then drives the post-write poll's shape: one atomic metadata rewrite (the
tmp-file rename every thread-metadata writer performs) dirties the signature, and the
sweep that follows is the deep probe — one cold pass, then five timed post-write sweeps,
victim rotation resolved once so the pick itself stays out of the timing. Evidence while
the live server runs older code points the same collector at the branch checkout
(`CHECKOUT` at the worktree root), the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core import sidebar_state
from src.core.sessions import probe_sidebar_state_sync, selective_probe_sidebar_state

# Worst deep-probe corpus: the session whose threads dir carries the most
# metadata files (the M5 resolution rule).
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    t = d / "threads"
    if t.is_dir():
        n = sum(1 for p in t.iterdir() if (p / "metadata.json").is_file())
        if n > best_n:
            best, best_n = d, n
SID = best.name
print(f"worst deep-probe corpus: session {SID}, {best_n} thread metadata files")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's threads/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m51-deep-probe-", dir="/tmp"))
dst = home / "sessions" / SID / "threads"
dst.parent.mkdir(parents=True)
shutil.copytree(best / "threads", dst)
spec = (SID, dst, home / "sessions" / SID / "triggers", home / "sessions" / SID / "plans.json")

sidebar_state.reset_for_tests()
probe_sidebar_state_sync([spec])  # cold pass, as at a server start; not timed

# Victim rotation, resolved once: each post-write sweep renames the next
# metadata file. (Picking the newest victim per round with a Path.glob would
# measure the pick, not the probe.)
victims = sorted(dst.glob("*/metadata.json"), key=lambda p: p.stat().st_mtime_ns)

def post_write_sweep() -> None:
    # One thread-metadata write dirties the session: the tmp inode's fresh
    # mtime_ns rides the atomic rename, the stat-only signature moves, and the
    # next poll's sweep deep-probes.
    victim = victims[post_write_sweep.i % len(victims)]
    post_write_sweep.i += 1
    tmp = victim.with_name("metadata.json.m51")
    tmp.write_text(victim.read_text(encoding="utf-8"), encoding="utf-8")
    os.replace(tmp, victim)
    entries, sigs = selective_probe_sidebar_state([spec], deep=False)
    assert entries, "deep probe returned no entry"
    for sid, sig in sigs.items():
        sidebar_state.store_probe_signature(sid, sig)  # the poll's on-loop storage half

post_write_sweep.i = 0
post_write_sweep()  # first post-write poll, as after any write; not timed
times = []
for _ in range(5):
    t0 = time.perf_counter()
    post_write_sweep()
    times.append(time.perf_counter() - t0)
times.sort()
print(f"{best_n} thread metadata files; post-write deep probe median "
      f"{times[2] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms")
shutil.rmtree(home)
EOF
```

M52 — chat-event append, per event. Every streamed turn appends one chat event per
delta through `save_chat_event` (`persist_and_broadcast` awaits it before each
broadcast), and every worker event lands through the same `append_ndjson` funnel;
the append is O(1) disk work but its per-call overhead rides the delta path. The
cost is write-side thread-pool time invisible to the read-side standing rows, so
the collector copies the session whose live chat file carries the most events into
a scratch `CHARLIEBOT_HOME` under /tmp (metadata.json and data/ only; live home
read once for the copy, never written), warms the events cache as a live streamed
turn does, and times 50 `save_chat_event` appends of one probe event, asserting
the appended lines parse back from disk in order. Evidence while the live server
runs older code points the same collector at the branch checkout (`CHECKOUT` at
the worktree root), the same shape as the M26 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, json, os, shutil, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# Worst append corpus: the session whose LIVE chat file carries the most
# events; a streamed turn appends one event per delta to exactly this file.
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

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only a copy of that
# session's metadata.json and data/; live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m52-append-home-", dir="/tmp"))
dst = home / "sessions" / SID
dst.mkdir(parents=True)
shutil.copy2(best / "metadata.json", dst / "metadata.json")
shutil.copytree(best / "data", dst / "data")

cfg = CharlieBotConfig(charliebot_home=home)
mgr = SessionManager(cfg)
mgr.load_chat_events_sync(SID)  # warm the events cache, as a live streamed turn does

probe_base = {"type": "assistant", "message": {"content": [{"type": "text", "text": "m52 probe chunk " + "y" * 400}]}}

async def main():
    times = []
    for i in range(50):
        ev = {**probe_base, "id": f"m52-probe-{i}", "timestamp": f"2026-09-04T19:01:{i:02d}Z"}
        t0 = time.perf_counter()
        await mgr.save_chat_event(SID, ev)
        times.append(time.perf_counter() - t0)
    times.sort()
    cached = mgr.load_chat_events_sync(SID)
    with open(home / "sessions" / SID / "data" / "chat_events.jsonl", "rb") as f:
        lines = f.read().split(b"\n")[:-1]
    on_disk = [json.loads(line) for line in lines[best_n:]]
    ids = [e["id"] for e in on_disk]
    parity = ids == [f"m52-probe-{i}" for i in range(50)] and len(cached) == best_n + 50
    print(f"{best_n}-event corpus; save_chat_event append median {times[24] * 1e6:.0f} us, "
          f"max {times[-1] * 1e6:.0f} us over 50; parity {parity}")
    shutil.rmtree(home)

asyncio.run(main())
EOF
```

M53 — config reload failure re-fire, broken steady state. `get_config` is the
per-request config read (the auth middleware calls it on every HTTP request,
the scheduler on every tick), and while the corpus stays broken a failed
reload re-ran the full YAML parse + model validation and re-fired
`config_reload_failed` on every call — the pre-fix form left `_config_mtime`
at the old fingerprint, so the reload condition never went false (the live
burst: 4431 lines in a 24.9 h server log, ~1/s inside the 16:00-18:00 window
of 2026-09-04, three distinct error strings). The fixed form memoizes the
failed reload on its fingerprint — re-parse only when a file moves, the same
freshness rule the success path follows — and routes the warning through a
warn-once registry, one line per error string per process, cleared on a
successful load so a relapse earns one new line. The cost is per-request work
invisible to the standing probes while the corpus is broken (a state the live
host entered for a 2 h window), so the collector seeds a good config in a
scratch `CHARLIEBOT_HOME`, breaks it with a fragment declaring a key the
model does not declare (the burst's error shape), and drives `get_config`:
one onset pass, as at the first call after the corpus breaks, then 60 timed
steady-state calls asserting the served instance's identity, then one
fingerprint-move round asserting the freshness survived. Evidence points the
same collector at the before and after checkouts (`CHECKOUT` at each root),
the same shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import os, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from src.core import config as core_config

# Broken-config corpus: config.yaml plus one fragment declaring a key the model
# does not declare — the live burst's error shape (unknown config key(s) ...).
# Scratch CHARLIEBOT_HOME; the live home is never read or written here.
work = Path(tempfile.mkdtemp(prefix="m53-reload-"))
home = work / "home"
(home / "config.d").mkdir(parents=True)
(home / "config.yaml").write_text("", encoding="utf-8")
os.environ["CHARLIEBOT_HOME"] = str(home)

core_config._config = None
core_config._config_mtime = 0.0
cached = core_config.get_config()  # seed: the running server's last-good config; not timed

(home / "config.d" / "broken.yaml").write_text("unknown_m53_key: 1\n", encoding="utf-8")

warns = []
parses = []
orig_warn = core_config.log.warning
orig_load = core_config.load_config
core_config.log.warning = lambda event, **kw: warns.append({"event": event, **kw})
core_config.load_config = lambda: (parses.append(1), orig_load())[1]
try:
    core_config.get_config()  # onset pass, as at the first call after the corpus breaks; not timed
    onset_warns, onset_parses = len(warns), len(parses)
    times = []
    for _ in range(60):  # steady-state repeat calls of the per-request auth path
        t0 = time.perf_counter()
        got = core_config.get_config()
        times.append(time.perf_counter() - t0)
        assert got is cached, "served config identity changed across a broken steady state"
    steady_warns, steady_parses = len(warns), len(parses)
    os.utime(home / "config.d" / "broken.yaml", (0, 0))  # fingerprint move: freshness must survive
    core_config.get_config()
    moved_warns, moved_parses = len(warns) - steady_warns, len(parses) - steady_parses
finally:
    core_config.log.warning = orig_warn
    core_config.load_config = orig_load
times.sort()
print(f"60 steady-state get_config calls of a persistently-broken corpus; config_reload_failed warnings "
      f"{steady_warns} (onset {onset_warns}), re-parses {steady_parses} (onset {onset_parses}); "
      f"call wall median {times[30] * 1000:.2f} ms, max {times[-1] * 1000:.2f} ms; "
      f"fingerprint-move round: re-parse {moved_parses}, new warnings {moved_warns}")
EOF
```

M54 — stream-draft paint work with the page's real highlight.js build. The
M33 harness stubs hljs, so the standing replay metric never sees
highlightAuto's cost: with the served 11.9.0 common build (36 languages),
highlightAuto scores the block against every language (~0.26 s per 24 KB
measured), and every streaming paint re-parses the whole draft, re-running it
on every unchanged code block in the draft. The collector loads the pinned
hljs build into the same harness and replays the largest on-disk assistant
draft containing a bare code fence — the corpus shape whose paint cost
highlightAuto dominates. Evidence points the collector at the before and
after checkouts (`CHECKOUT` at each root, live state read-only), the same
shape as the M18 protocol:

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} node /home/chaoli/workspace/charlie-bot/tests/stream_hl_render_collector.js
```

M55 — artifact compare-view serve, steady state. The plan panel's 对比上一版
toggle and every artifact `?diff=` link run the file server's annotate path:
the pre-fix handler read both pages and ran `plan_diff.annotate` inline on the
event loop per request — ~0.25 s of loop freeze per compare view of a 1 MB
pair (the M14 pathology), re-computing an immutable result on every repeat
view (the toggle flip, a plan update re-render, a refresh). The fixed handler
builds the page in one thread hop and memoizes it on both files' (path,
mtime_ns, size) signatures plus the injection flag — the marks are a pure
function of the two files' bytes and artifact pages are only ever written
whole, so a repeat view re-runs zero annotate. The cost is a per-click latency
no standing probe covers, so the collector snapshots the worst artifact pair
(the session whose artifacts dir carries the most .html bytes; target =
biggest page, base = runner-up) into a scratch `CHARLIEBOT_HOME` under /tmp
(live home read once for the copy, never written) and drives the route through
TestClient in each checkout's process: one cold pass, as at first compare-view
open, then nine timed repeats, asserting byte-identical bodies. Evidence
points the same collector at the before and after checkouts (``CHECKOUT`` at
each root, shared snapshot home), the same shape as the M35 protocol.
Snapshot once:

```bash
/home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import shutil, tempfile
from pathlib import Path

# Worst artifact-pair corpus: the session whose artifacts dir carries the most
# .html bytes; target = biggest page, base = runner-up (the compare-view pair).
root = Path.home() / ".charliebot" / "sessions"
best, best_n, target, base = None, -1, None, None
for d in root.iterdir():
    art = d / "artifacts"
    if not art.is_dir():
        continue
    pages = sorted(art.glob("*.html"), key=lambda p: p.stat().st_size, reverse=True)
    if len(pages) < 2:
        continue
    n = sum(p.stat().st_size for p in pages)
    if n > best_n:
        best, best_n = d, n
        target, base = pages[0].name, pages[1].name
print(f"worst artifact pair: session {best.name}, {best_n / 1e6:.1f} MB html "
      f"({target} vs {base})")

# Isolation: scratch CHARLIEBOT_HOME under /tmp holding only the copied pair;
# live home read once for the copy, never written.
home = Path(tempfile.mkdtemp(prefix="m55-artifact-home-", dir="/tmp"))
dst = home / "sessions" / best.name / "artifacts"
dst.mkdir(parents=True)
shutil.copy2(best / "artifacts" / target, dst / target)
shutil.copy2(best / "artifacts" / base, dst / base)
print(f"export M55_HOME={home} M55_SID={best.name} M55_TARGET={target} M55_BASE={base}")
EOF
```

Then run per checkout (``eval`` the snapshot export first):

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import hashlib, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from fastapi.testclient import TestClient
from src.api.files import router as files_router
from src.core.config import CharlieBotConfig
import src.api.files as files_mod

home = Path(os.environ["M55_HOME"])
SID = os.environ["M55_SID"]
TARGET = os.environ["M55_TARGET"]
BASE = os.environ["M55_BASE"]

# Scratch wiring: the router's get_config resolves the snapshot home, never the
# live one; the snapshot's empty access key makes every reader credentialed, so
# the timed request carries the artifact-comments injection like a real view.
cfg = CharlieBotConfig(charliebot_home=home)
files_mod.get_config = lambda: cfg
app = FastAPI()
app.include_router(files_router, prefix="/files")
client = TestClient(app)
# The file server addresses pages by absolute filesystem path (the /files and
# /absolute_filepath prefixes are aliases); ?diff= stays session-relative.
url = f"/files/{home}/sessions/{SID}/artifacts/{TARGET}?diff=artifacts/{BASE}"

t0 = time.perf_counter()
r = client.get(url)  # cold pass, as at first compare-view open; not timed
cold = time.perf_counter() - t0
assert r.status_code == 200, (r.status_code, r.text[:200])
assert "对比上一版" in r.text, "annotated page missing the compare header"

times = []
bodies = set()
digest = ""
for _ in range(9):
    t0 = time.perf_counter()
    r = client.get(url)
    times.append(time.perf_counter() - t0)
    bodies.add(len(r.content))
    digest = hashlib.sha256(r.content).hexdigest()[:12]
times.sort()
assert len(bodies) == 1, f"repeat bodies differ: {bodies}"
print(f"{TARGET} vs {BASE}; first view {cold:.4f} s; repeat-view median {times[4]:.4f} s, "
      f"max {times[-1]:.4f} s over 9, body {bodies.pop()} B, digest {digest}")
EOF
```

M56 — sidebar status poll, steady state. The sidebar polls `GET /api/sessions/status?ids=…` every
3 s per open dashboard tab with the sessions it renders (this host's second-busiest route after
the workers-panel list); the handler resolves every id's metadata plus the derived sidebar state
and the pre-fix mapped return paid FastAPI's jsonable_encoder pass over the 41-row dict. The cost
is per-poll latency invisible to the standing HTTP probes (M3 reads the 401 floor), so the
collector drives the endpoint through TestClient over the live corpus (read-only), ids resolved
from the active-session listing, from the checkout under test: one cold pass, as at first
sidebar paint after a server start, then nine timed requests, with a parsed-body digest so a
corpus change between arms cannot masquerade as a payload difference.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import asyncio, hashlib, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from fastapi.testclient import TestClient
from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.sessions import SessionManager

# The sidebar polls the sessions it renders; the active set is that corpus.
async def ids():
    cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
    mgr = SessionManager(cfg)
    metas = await asyncio.to_thread(mgr.list_active_session_metas)
    return ",".join(m.id for m in metas)

IDS = asyncio.run(ids())

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
mgr = SessionManager(cfg)
app = FastAPI()
app.include_router(sessions_router, prefix="/api/sessions")
app.dependency_overrides[get_session_manager] = lambda: mgr
client = TestClient(app)
url = f"/api/sessions/status?ids={IDS}"

def digest(body):
    return hashlib.sha256(json.dumps(json.loads(body), sort_keys=True).encode()).hexdigest()[:12]

client.get(url)  # cold pass, as at first sidebar paint after a server start; not timed
times, body = [], None
for _ in range(9):
    t0 = time.perf_counter()
    r = client.get(url)
    times.append(time.perf_counter() - t0)
    body = r.content
times.sort()
print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: {len(IDS.split(','))} sidebar ids; "
      f"/status request median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms, "
      f"body {len(body)} B, digest {digest(body)}")
EOF
```

M57 — plan-registry poll, steady state. The plan panel polls `GET /api/sessions/{id}/plans` every
3 s while open; M27 memoized the registry read itself (10.6 µs steady state) but the endpoint's
mapped dict return still paid FastAPI's jsonable_encoder pass over the 12-plan payload on every
poll. The cost is per-poll latency invisible to the standing HTTP probes, so the collector drives
the endpoint through TestClient over the worst on-disk plans corpus (the session whose plans.json
carries the most bytes, live state read-only), from the checkout under test: one cold pass, as at
first panel paint after a server start, then nine timed requests, with a parsed-body digest.

```bash
CHECKOUT=${CHECKOUT:-/home/chaoli/workspace/charlie-bot} /home/chaoli/workspace/charlie-bot/.venv/bin/python - <<'EOF'
import hashlib, json, os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ["CHECKOUT"])
from fastapi import FastAPI
from fastapi.testclient import TestClient
from src.api.deps import get_plan_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.plans import PlanRegistryManager
from src.core.sessions import SessionManager

# Worst plans corpus: the session whose plans.json carries the most bytes.
root = Path.home() / ".charliebot" / "sessions"
best, best_n = None, -1
for d in root.iterdir():
    p = d / "plans.json"
    if p.is_file():
        n = p.stat().st_size
        if n > best_n:
            best, best_n = p, n
SID = best.parent.name

cfg = CharlieBotConfig(charliebot_home=Path.home() / ".charliebot")
mgr = SessionManager(cfg)
plan_mgr = PlanRegistryManager(cfg, mgr)
app = FastAPI()
app.include_router(sessions_router, prefix="/api/sessions")
app.dependency_overrides[get_plan_manager] = lambda: plan_mgr
client = TestClient(app)
url = f"/api/sessions/{SID}/plans"

def digest(body):
    return hashlib.sha256(json.dumps(json.loads(body), sort_keys=True).encode()).hexdigest()[:12]

client.get(url)  # cold pass, as at first panel paint after a server start; not timed
times, body = [], None
for _ in range(9):
    t0 = time.perf_counter()
    r = client.get(url)
    times.append(time.perf_counter() - t0)
    body = r.content
times.sort()
print(f"checkout {os.environ['CHECKOUT'].rsplit('/', 1)[-1]}: session {SID}, {best_n / 1e3:.1f} KB plans.json; "
      f"/plans request median {times[4]*1000:.2f} ms, max {times[-1]*1000:.2f} ms, "
      f"body {len(body)} B, digest {digest(body)}")
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
| 2026-09-02 | #615 | M8 append-round absent-needle search median 0.1797 s → 0.0021 s, max 0.1901 s → 0.0025 s (scratch CHARLIEBOT_HOME A/B over the 5 biggest active sessions, 109.2 MB, one event appended to every file before each timed round; main checkout before at load 0.87/2.71/3.37 vs branch after at load 0.97/1.51/2.61; cold full scan 0.1975 s → 0.1466 s under the same load shift; absent-round and positive-round parity both arms) | proven-absent memo signature gains the inode, and a same-inode file that grew re-proves absence from a window over the appended tail (old_size − 4·len(query) − 8 seek) instead of a full re-scan — chat files mutate only by append between inode-swapping atomic archive rewrites; whole-file path keeps strict decoding |
| 2026-09-02 | this PR | M34 events poll full fetch 0.0097 s / 860706 B → after=total 0.0035 s / 40 B (collector verbatim, 6.7 MB / 2177-event worst on-disk log, scratch CHARLIEBOT_HOME TestClient, main full arm vs branch full arm back-to-back at load 0.43/0.66/0.86, payload sha256-identical deb85be56bf4dbba; live-before corroboration 0.010 s / 860706 B warm from the running instance; prefix+tail reconstruction parity true, envelope rows byte-equal to plain-list rows) | events endpoint gains after=N envelope (append-only prefix cut on the projection, reset+full payload when the count is ahead) served through a model_dump JSONResponse — FastAPI's jsonable_encoder fallback for mapped returns measures 6x slower on the same list (48.8 ms vs 8.1 ms); client polls pass the rendered raw count and append tails via scratch paint + insertAdjacentHTML; M34 definition and healthy range introduced with this PR; M18 collector element stub gains dataset to match the real DOM |
| 2026-09-02 | this PR | M35 events page median 21.99 ms → 14.75 ms, max 24.88 ms → 16.00 ms (558888 B, 211 msgs); view median 17.26 ms → 15.55 ms; bootstrap median 8.41 ms → 6.54 ms (collector verbatim, 20534-event worst live corpus snapshot, shared scratch CHARLIEBOT_HOME TestClient A/B, main checkout before vs final branch head after back-to-back at load 1.27/1.08/0.87 and 1.39/1.11/0.88; all three bodies sha-identical across arms: events f552ad6de73b, view 33e20ccff76d, bootstrap 61b3ca97cd5e) | events/view/bootstrap message-page handlers return JSONResponse directly, skipping FastAPI's jsonable_encoder pass (~3x a plain json.dumps on the 559 KB page); M35 definition and healthy range introduced with this PR |
| 2026-09-02 | #628 | M17 fork median 0.2178 s → 0.1769 s, medians of three interleaved verbatim-collector rounds (5519 parent events, 36.3 MB corpus, scratch CHARLIEBOT_HOME A/B, main checkout before vs final branch head after at load 4.7-5.8; every paired round faster: 0.324 → 0.211, 0.218 → 0.177, 0.190 → 0.140; reference bytes byte-identical across arms on the corpus and on 12 synthetic framing shapes; earlier light-load phase profile: per-line str strip+join+encode ~190 ms of the fork's 140 ms) | full-corpus reference streams raw line bytes per source file into the atomic tmp sibling (new atomic_write_stream in json_utils) instead of decode→strip→join→encode in one big str: bulk numpy `{}`-shape check answers the common all-plain shape with one window write, per-frame fallback keeps CR folding and corrupt-line rejection; utf-8 validity keeps the text-mode read's UnicodeDecodeError parity |
| 2026-09-02 | this PR | M36 poll median 15.60 ms → 7.98 ms, max 48.64 ms → 8.75 ms, body 1799215 B → 131896 B (collector verbatim, 1963 KB / 266-row worst worker-list corpus, 265 rows truncated after; main checkout before vs final branch head after back-to-back at load 0.91/0.98/1.02 and 1.00/1.00/1.02; card renders the identical one-line prefix; full text fetches on modal click only; earlier same-corpus round median 16.37 ms before vs 8.27 ms after at load ≤1.0) | workers-panel list rows ship a 240-char description prefix plus a description_full_len marker instead of whole task-spec-length descriptions; the full-text modal fetches the thread row on demand; M36 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M7 live-before median 15.884 s, max 21.738 s (busy host, load 5.53/7.30/11.03; 16.4 GB db whose WAL moves every ~5 s, so the file-signature entry misses every load) and max 16.801 s in the quiet re-pair → scratch-after warm median 0.422 s, max 0.437 s (scratch server on the branch with an empty scratch CHARLIEBOT_HOME, cold 20.996 s; verbatim M7 commands back-to-back at load 1.07/0.91; collector-level over the live cache doc: opencode rescan 12.928 s → 0.164 s, warm collect total 16.996 s → 0.49 s) | per-row (id, time_updated) memo over the opencode message table: a leaf-page key diff plus per-id fetch of moved rows replaces the whole-table re-scan per WAL-invalidated load; rows the scan SQL filters out memoize as non-contributors; record parity pinned by the suite |
| 2026-09-03 | this PR | M37 steady-state median 9.12 ms → 0.01 ms, max 12.30 ms → 0.03 ms (collector verbatim, 7.4 MB / 3081-line worst archived-session live file of session 3b91d606, live state read-only, main checkout before at load 1.72/2.44/2.12 vs branch-after at load 1.23/1.69/1.86, back-to-back; served tail page repeat-identical in both arms) | whole-page (mtime_ns, size) memo for parse_ndjson_tail plus a shared (mtime_ns, size) memo for count_ndjson_lines, both keyed on the pre-read signature so an entry recorded during a concurrent append can never be served for the newer bytes; M37 definition and healthy range introduced with this PR |
| 2026-09-03 | #646 | M36 poll median 13.92 ms → 5.46 ms, max 18.28 ms → 6.43 ms, body 137363 B in both arms (collector verbatim, 2051 KB / 277-row worst worker-list corpus, live state read-only, main checkout before vs branch head after back-to-back at load 3.81/2.27/1.64 and 2.76/2.17/1.63; 100-rep interleaved corroboration min 5.79 ms → 3.19 ms) | whole-body memo for the 3 s workers-panel list poll keyed on the union (path, mtime_ns, size) signature of every thread metadata.json and trigger *.json behind the rows (all writers rename atomically, so an unchanged signature proves the body current); steady-state polls skip row building and JSON serialization |
| 2026-09-03 | this PR | M38 turn replay 174 → 2 stream frames, 286 → 114 json.dumps calls, 78 ms → 5 ms dumps total, replay wall 0.08 s → 0.01 s (collector verbatim, session ebace12d worst on-disk stream turn by preview bytes, instant feed, one subscriber, main checkout before at load 3.02/5.02/5.05 vs branch head after at load 1.74/4.20/4.76; final-frame parity true both arms) | per-channel leading+trailing coalescing of stream preview frames in StreamingManager at the client's 200 ms paint cadence, pending draft dropped only on preview-hiding frame types (message/assistant_error/error), one json.dumps per fan-out replacing one per subscriber, subscriber-less channels short-circuited to a dict lookup; M38 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M21 steady-state sweep median 0.0088 s → 0.0045 s, max 0.0092 s → 0.0048 s (collector verbatim, 38 active sessions, live state read-only, main checkout before vs branch head after back-to-back at load 0.76/1.50/1.31; signature-pass microbench 8.8 ms → 4.0 ms with signatures identical over every active session) | os.scandir + str-joined os.stat in the sidebar probe-input signature pass, replacing per-entry Path()/__truediv__ allocation whose parse overhead measured ~half the sweep, mirroring the M29/M32 str-stat pattern |
| 2026-09-03 | this PR | M39 inline loop-lag median 15.34 ms → 5.34 ms (5 ms ticker floor), max 16.24 ms → 5.47 ms; busy-check wall median 15.34 ms → 0.26 ms, max 16.24 ms → 0.43 ms (collector verbatim, 942-dir ~/.claude/projects corpus, synthetic never-present id, main checkout before at load 0.25/0.34/0.74 vs branch head after at load 0.84/0.80/0.82; busy result False in both arms) | per-session transcript-path memo in _find_existing_claude_jsonl (stable hit memoized for the process life with an exists() recheck, miss re-globbed at a 30 s TTL) plus the tui/status busy check awaited in a thread instead of inline on the event loop; M39 definition and healthy range introduced with this PR |
| 2026-09-03 | #665 | M40 starred list median 12.80 ms → 3.25 ms, group-name reduction median 13.63 ms → 3.05 ms; maxima 35.92 ms → 25.02 ms / 14.20 ms → 3.82 ms (collector verbatim, main checkout before vs branch head after back-to-back at load 2.90/2.82/2.16 and 3.15/2.90/2.22; live 1012-meta corpus read-only, 10 starred rows, 25 groups; list outputs identical; the residual starred maxima are dirty deep-probe spikes in the unchanged enrich path) | starred/scheduled filters run against the shared cached metas (read-only) before the model_copy+thinking stamp, so only surviving rows pay the leaving-the-manager copy; /api/sessions/groups and the autonamer's group list go through list_group_names, a copy-free read-only reduction of the cached metas; M40 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M41 first view 0.1781 s → 0.1217 s, repeat-view median 0.1766 s → 0.0063 s, max 0.1874 s → 0.0069 s (collector verbatim, 492 files in the charlie-bot root..HEAD diff, main checkout before at load 1.20/1.21/0.98 vs branch after at load 0.49/0.89/0.93, back-to-back; manifest body sha256-identical ed990917efcc across arms; M14 loop-lag re-measured unchanged, 0.0058 s → 0.0060 s at the 5 ms ticker floor) | git diff/files manifest memoized on (repo, resolved base/head SHAs, mode, .gitattributes signature) — a commit-pair diff is immutable — plus both refs resolved in one rev-parse and the two manifest diffs run concurrently on a miss; M41 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M42 steady-state tick loop-lag median 0.0135 s → 0.0060 s (the 5 ms ticker floor), max 0.0146 s → 0.0071 s (collector verbatim, 12 enabled tasks over the 1012-session live corpus, fire stub awaited 0x in both arms, main checkout before vs final branch head after back-to-back at load 1.06/1.16/1.16; first standalone round 0.0102 s before) | scheduler tick's per-task session cache built via the M40 scheduled=True pre-copy filter: only the 52 scheduled rows pay model_copy+thinking stamp instead of all ~1012 cached metas; M42 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M43 repeat-expand median 0.0441 s → 0.0061 s, max 0.0541 s → 0.0063 s; first expand 0.0549 s → 0.0503 s (collector verbatim, heaviest charlie-bot root..HEAD manifest file web/static/css/tailwind.css +2515/-0, 45756 B, main checkout before vs branch head after back-to-back at load 1.20/1.13/1.02; bodies repeat-identical in both arms; live log corroboration 8 diff/file requests in the 24.4 h server log, one a re-expand) | per-file diff body memoized on the M41 manifest key plus the pathspec, miss-path range spec built from the resolved SHAs; M43 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M29 steady-state median 1.67-1.96 ms → 0.84-0.90 ms across five interleaved verbatim-collector rounds (1019 session dirs, 44 active, live corpus read-only, main checkout before vs branch after back-to-back at load 1.2-2.0; listings identical in both arms; earlier same-day round under the sibling CUDA build's load 7-12 read main 6.08 ms vs the <5 ms healthy range, recovering to 1.69-1.96 ms once the build drained at identical code — load noise, no regression) | session-dir name list memoized on the sessions root's own (mtime_ns, size), signature taken before the scandir: session create/delete is what moves the root's mtime (metadata writes land one level below), so an unchanged signature proves the name set current and steady-state listings pay one stat instead of the ~1 ms per-1000-entry scandir |
| 2026-09-03 | this PR | M44 steady-state median 5.11 ms → 2.90 ms, max 8.63 ms → 6.29 ms (collector verbatim, 12 scheduled rows / 13194 B body, live session + cron corpus read-only, main checkout before vs branch after back-to-back at load 0.36/1.28/2.58 and 0.49/1.29/2.57; body digest identical ec0f7b654411 both arms; component corroboration: croniter get_next measured 2.97 ms over the 12-task corpus, one expand each) | per-(cron, timezone) memo for the /scheduled rows' next-fire resolution, entries valid until the fire time they name passes — get_next is a pure function of (cron, timezone, now) and no occurrence can land before that first next fire, so repeat requests inside the window recompute an identical string; M44 definition and healthy range introduced with this PR |
| 2026-09-03 | this PR | M7 collector-level warm collect median 0.3767 s → 0.0729 s, max 0.4274 s → 0.3471 s (verbatim warm-collect command, 16 rows / 3 notes, main checkout before at load 1.43/1.50/1.40 vs branch after at load 1.48/1.51/1.40 back-to-back; rows+notes digest agreement across interleaved arms whenever the live corpus held still between them — opencode db WAL moves every few seconds under live traffic; the after max is one such WAL-moved memo miss still paying the old fresh path; suite pins hit serves the first collect's rows/notes with zero scanned bytes) + HTTP-level live-before warm median 0.336 s → scratch-after warm median 0.056 s (verbatim M7 curls, live-before at load ~1.75, scratch server on the branch with a scratch CHARLIEBOT_HOME, cold 9.25 s) | whole-tally memo keyed on the walk signature plus the opencode db signature serves repeat collects without the cache-document JSON parse (~0.09 s), the apply(t) record replay (~43.5k add calls, ~0.075 s) or the db open per load, and the signature walk itself switches to str joins + raw os.stat (Path construction measured over twice the stat syscall on this corpus), with one shared os.walk error-hook home for both walkers |
| 2026-09-04 | this PR | M45 stale-cursor replay loop-lag median 0.0259 s → 0.0102 s, max 0.0281 s → 0.0102 s (collector verbatim, 20534-event worst live corpus at cursor 20484, 47 frames replayed, scratch CHARLIEBOT_HOME A/B, frame-list digest identical 314dfbe9fd89 across arms; main checkout before vs branch after back-to-back at load 1.77/1.32/0.91 and 1.79/1.33/0.91; replay wall 0.0259 s → 0.0242 s — the walk's CPU still runs, now off the loop; suite pins ws.sent == _catchup_frames output and the stop-at-first-failure send count) | session-WS catchup replay's full-history aggregator walk moved off the event loop: `_catchup_frames` builds the ordered frame list via asyncio.to_thread and `_replay_aggregated_catchup` only sends it in order; M45 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M46 GET /api/cron/tasks body 96235 B → 3745 B (resolved-prompt bytes 90348 → 0 of 12 task rows), handler median 2.71 ms → 1.90 ms, max 3.12 ms → 2.31 ms (collector verbatim, live cron corpus read-only through TestClient, main checkout before at load 0.40/0.77/0.82 vs branch head after at load 1.02/1.22/0.93, back-to-back; row keys identical minus prompt; live-log corroboration 96 KB fetch bodies) | cron tasks list dump excludes prompt, the resolved body the in-process scheduler/master reads while every consumer of the route edits prompt_file (POST/PUT responses never carried it), mirroring M36's description prefix cut; M46 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M47 60 → 0 claude_declared_window_degraded warnings per 60 steady-state resolutions (collector verbatim, main checkout before vs branch after; live-log corroboration 62 lines in the 7.89 h server log ≈ 8/h, host exports CLAUDE_CODE_MAX_CONTEXT_TOKENS=400000) | declared-window degradation warnings routed through an (event, variable, value) warn-once guard — one line per degradation per process, a swapped bad value earns one new line; M47 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M48 60 → 0 search_read_failed debug lines per 60 steady-state content scans of a no-live-file session (collector verbatim, main checkout before vs branch after back-to-back at load 0.75/0.78/0.64; live-log corroboration 30 lines in the 11.92 h server log, all naming the one active session with no live chat file; search results and M8 monitoring shape unchanged) | both search content-scan failed-read loggers (the stat failure and the scan-open failure) routed through a (session, error) warn-once guard — one line per reported failure per process, a swapped path or errno earns one new line; M48 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M49 60 → 0 opencode_part_unhandled debug lines per 60 steady-state parts of one unhandled type (collector verbatim, main checkout before vs branch after back-to-back at load 0.25/0.59/0.86; live-log corroboration 152 lines in the 12.88 h server log, every line type=patch; unhandled parts still translate to []) | the part type falling through `_translate_part` routed through a part-type warn-once registry — one line per unmapped type per process; M49 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M50 60 → 0 ext_usage_no_access_token warnings per 60 steady-state credential reads of a tokenless file (collector verbatim, main checkout before at load 0.97/0.75/0.66 vs branch after at load 0.29/0.59/0.62, back-to-back; live-log corroboration 135 lines in the 13.89 h server log ≈ 10/h, every line ext_usage_no_access_token naming the same path; every read still returns None and a token-bearing read re-arms the path) | both `_read_credentials` failure-site warnings (ext_usage_credentials_not_found, ext_usage_no_access_token) routed through a recovery-aware warn-once registry — one line per (event, path) per broken streak, a read returning a token re-arms the path; M50 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M51 post-write deep probe median 25.82/25.62 ms → 6.12/6.30 ms, max 26.22/25.98 ms → 6.33/8.04 ms (collector verbatim, 339-file worst threads corpus, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×2 at load 1.16-1.70/1.18-1.32/0.95-1.04; probe verdicts identical — unchanged-sig sweep re-measured 0.0021 s vs 0.0022 s standing M21, no regression) | the sidebar deep probe's thread-metadata scan (`iter_recent_thread_metas`, shared with the boot recovery scan) memoizes each in-window metadata file's parsed dict on (path, mtime_ns, size) — every writer publishes through the atomic tmp-file rename so a content change always moves mtime_ns, the signature is taken before the read so a mid-rewrite entry keys the older signature, only successful parses memoize, and yielded dicts are shared read-only; a post-write probe re-parses the moved file alone instead of all 339; M51 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M36 unchanged-poll body 168209 B → 0 B (conditional ?etag= repeat answers 204; collector with the conditional round, 2551 KB / 339-row worst worker-list corpus, live state read-only, main checkout before vs branch head after back-to-back ×2 at load 0.85/1.13/0.89 and 1.42/2.84/2.64; full-poll first-paint path unchanged — medians 4.27/4.66/6.09 ms → 4.25/5.06/4.57 ms with the byte-identical 168209 B body; live read-only check confirms the running instance still serves 200 full) | the list body carries a strong content-addressed ETag (sha1 of the body bytes) plus Cache-Control: no-store; the poll repeats the ETag it rendered via ?etag= and the whole-body memo's unchanged signature serves a bodyless 204 — the client keeps its rendered rows and skips the 339-row JSON.parse while nothing behind the list moved (a query param, not If-None-Match, because the browser's HTTP cache fulfils a revalidation itself and fetch never surfaces the 304); conditional sub-metric added to the M36 definition and collector in this PR |
| 2026-09-04 | this PR | M52 save_chat_event append median 346/382 µs → 176/184 µs, maxima 2353/2477 µs → 1977/1963 µs (collector verbatim, 20534-event worst live corpus, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×2 at load 1.25/1.79/2.09; appended lines parse back in order both arms; isolated-component check: raw open+write+close inline 19 µs, one executor round-trip ~104 µs under the same load) | chat-event appends left aiofiles' open+write+close — three executor round-trips, ~355 µs — on the streamed-turn delta path (one append per stream delta, each gating its broadcast; a 500-delta turn paid ~180 ms of append overhead); one to_thread hop around a raw open(O_APPEND)+write+close keeps the off-loop write rule at one round-trip, O_APPEND re-resolves the path per call so an atomic archive rewrite or recreate never lands behind a stale handle, and the write loop keeps the io stack's write-all contract; the same funnel serves the worker events-log appends (src/agents/worker.py); M52 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M30 append-round 8-page scroll median 0.0406/0.0417 s → 0.0003/0.0003 s, max 0.0515 s → 0.0005 s (collector + append-round rounds, 9.4 MB live file of archived session 3b91d606, archive_offset 1136, scratch CHARLIEBOT_HOME A/B, main checkout before vs branch after interleaved back-to-back ×2 at load 0.65-1.93; live-half event count 3840 identical across all four arms; unchanged-file steady state re-measured 0.0002 s both arms, no regression) | the live-half memo extends on a same-inode size growth, re-parsing only the appended tail instead of the whole file — chat files mutate only by append between archive rewrites and the rewrite publishes through os.replace (new inode), so the inode rules out reading a rewrite as append growth; the covered byte offset tracks the bytes actually parsed (an entry whose read raced a landing append keys its pre-read stat and stays reachable only as an extension base), and a covered content ending mid-line blocks extension until a full re-parse lands on a line boundary, so a completed append is never glued onto a half-parsed last line; append-round sub-metric added to the M30 definition and collector in this PR |
| 2026-09-04 | this PR | M3 standing collector median 0.001 s, max 0.001 s (unchanged; live server runs pre-fix code, flagged); in-process raw-ASGI A/B of the real app stack, identical harness command both arms (import server, drive GET /api/sessions/status, 300 timed calls after 20 warm), main checkout before vs branch after back-to-back ×2 at load 1.67-1.78: 401 no-auth path median 137/141 µs → 81/78 µs, max 717/923 µs → 111/156 µs; 200 bearer path (one live session id) median 599/615 µs → 425/433 µs, max 1008/974 µs → 970/767 µs | AuthMiddleware rewritten from BaseHTTPMiddleware to pure ASGI — the wrapper's per-request task plus anyio memory-stream round-trip were the middleware floor's dominant slice (M3's timing target); the 401 branch synthesizes its HTML/JSON responses by hand and the authorized branch forwards the scope untouched |
| 2026-09-04 | this PR | M7 live-before warm median 0.327 s and 0.751 s across two rounds, max 0.926 s (verbatim M7 curls against the running instance at load 0.82-0.91) → scratch-after warm median 0.056 s, max 0.574 s (scratch server on the branch with a scratch CHARLIEBOT_HOME, cold 9.516 s; verbatim M7 curls back-to-back with the live rounds); in-process forced-WAL-miss A/B: before median 0.3557 s → after median 0.1406 s (collect_token_usage interleaved back-to-back over the live corpora with the db signature forced to move every round — the trigger a WAL write produces, rows untouched; rows+notes digests identical on the 8 signature-stable rounds of 9, one skipped round's corpus moved mid-pair) | whole-tally memo gains a second hit tier keyed on the row memo's change epoch: the moved-WAL miss pays one incremental key-diff scan instead of the 12.9 MB cache-document load plus the ~50k-record opencode replay, an unchanged epoch re-serves the memo rows and re-signs them at the scan's own signature, and the cache document loads only on the source-walk path that needs it |
| 2026-09-04 | this PR | M35 events page median 9.29/9.41/8.59 ms → 6.10/5.58/5.61 ms (633236 B), view median 8.51/8.66/8.32 ms → 6.89/6.88/6.50 ms, bootstrap median 3.69/3.96/3.76 ms → 3.39/3.39/3.30 ms (three interleaved rounds of the verbatim collector, main checkout before vs branch after back-to-back on the shared M35 snapshot at load 4.9-5.8; every paired round faster; parsed bodies equal across arms — raw bodies differ by design, \uXXXX escaping) | the five hot JSONResponse sites (events pages, view, bootstrap, worker-events envelope) render through FastJsonResponse, whose render is a plain ASCII-escaped dumps — CPython's C JSON encoder is ~3x faster with ensure_ascii=True on CJK-bearing payloads (render 5.4 vs 1.9 ms measured on the 559 KB page; the sessions corpus here is Chinese-heavy) and never slower on ASCII-only ones; NaN/Infinity still raise at render time, and the M34 steady-state envelope re-measured unchanged (0.0024 s median) |
| 2026-09-04 | this PR | M51 post-write deep probe median 6.83/7.01/6.26 ms → 3.47/3.62/3.32 ms, maxima 7.08/7.71/6.66 ms → 3.62/4.51/3.38 ms (collector verbatim, 339-file worst threads corpus of session 3b91d606, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×3 at load 2.5-3.4, every paired round faster; M21 unchanged-signal sweep re-measured 0.0023 s vs the 0.0038 s standing row, no regression) | the deep probe's thread-metadata scan (`iter_recent_thread_metas`, shared with the boot recovery scan) walks scandir's plain strings — `entry.path + "/metadata.json"` into raw `os.stat`, str memo keys, string yields, Path built only at the rare content read and the interrupted-run collection point — dropping the two per-entry Path allocations plus the Path.stat() indirection that measured 5.4 ms of the 339-file walk against 1.3 ms for the same files through raw os.stat, the same conversion the M21 signature pass took |
| 2026-09-04 | this PR | M53 61 → 1 warnings and 61 → 1 re-parses per 61 calls (onset 1 + 60 steady-state, collector verbatim, scratch broken-config corpus, main checkout before vs branch after interleaved back-to-back ×2 at load 3.03-3.12; call wall median 0.45/0.46 ms → 0.08/0.08 ms on the collector's minimal corpus, with the live corpus's full parse measured at 9.25 ms — the per-request cost while the live home was broken; fingerprint-move round re-parse 1 both arms, new warnings 1 → 0; served config identity `is`-asserted across all 60 steady-state calls both arms; live-log corroboration 4431 config_reload_failed lines in the 24.9 h server log, ~1/s inside the 16:00-18:00 burst window, three distinct error strings) | get_config memoizes the failed reload on its fingerprint — re-parse only when a file moves, the same freshness rule the success path follows, so an unchanged broken corpus costs one fingerprint stat set per call instead of a full parse + warning per call — and routes config_reload_failed through a warn-once registry, one line per error string per process, cleared on a successful load so a later relapse earns one new line; M53 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M54 paint-work median 0.299/0.323 s → 0.141/0.142 s, maxima 0.390/0.406 s → 0.219/0.236 s (collector verbatim, 11.4 KB worst fence-bearing on-disk assistant draft sha1 b155f860788f, 59 deltas at 40 ms virtual cadence, 12 paints, page-pinned marked + hljs 11.9.0 common build (36 languages), main checkout before vs branch after interleaved back-to-back ×2 at load 2.24/1.15/0.80; final-frame parity true both arms; M33 stubbed-hljs replay re-measured 0.396 s median vs the 0.378 s standing row at a higher load, no regression) | highlight results memoized in renderer.code on (lang, code) with a 32-entry LRU — highlight is a pure function of its inputs, but every streaming paint re-parsed the whole draft and re-ran highlightAuto (a 36-language scoring pass, ~0.26 s per 24 KB measured) on every unchanged code block, and every message re-render paid it again; M54 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M8 interleaved-family warm search: family B median 145.22 ms → 4.08 ms, family A 4.78 ms → 3.99 ms (two unrelated absent-needle families alternating over 6 interleaved rounds, collector over the identical 155 MB / 40-file snapshot corpus, main checkout before vs branch after back-to-back; rows 0 both families both arms; content scans across the 12 timed rounds 240 → 0 (collector totals 320 → 80 including the two cold passes); live corroboration: the running instance re-reads 151 MB per repeat search (rchar delta) and answers in 74-160 ms because its one-slot memo is occupied by shorter real-search needles) | the content-search miss memo keeps a per-file LRU of proven-absent roots (needle → signature, cap 8) instead of one shortest-needle slot — the one-slot form let the shortest needle ever searched permanently occupy the proof and sent every query family outside its superstrings back to a full 155 MB corpus scan per request |
| 2026-09-05 | this PR | M55 artifact compare-view repeat median 0.2484/0.2675 s → 0.0028/0.0025 s, maxima 0.2706/0.3052 s → 0.0035/0.0031 s (collector verbatim, 1.5 MB worst artifact pair understanding_packed-batch-cost-balance_v10.html vs _v9.html, scratch CHARLIEBOT_HOME, main checkout before vs branch after interleaved back-to-back ×2 at load 0.89-0.97; served body byte-identical across arms — 150741 B, digest a82879fdc034; first view unchanged at 0.2270-0.2532 s both arms, now off the loop) | the `?diff=` annotate moved off the event loop into one thread hop and its result memoized on both files' (path, mtime_ns, size) signatures plus the injection flag — the marks are a pure function of the two files' bytes and artifact pages are only ever written whole, so a repeat compare view re-runs zero annotate; the pre-fix inline annotate froze the event loop 246.2 ms per repeat request (raw-ASGI 5 ms-ticker round: loop-lag 246.2 ms, wall 246.6 ms before vs 5.2 ms / 1.6 ms after), the same pathology M14 measured on the git diff endpoints; the clean artifact view's comment-layer injection also left the event loop; M55 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M56 /status request median 3.03/2.89 ms → 2.30/2.37 ms, maxima 8.57/8.55 ms → 7.65/7.72 ms (collector verbatim, 41 active-session ids, live corpus read-only, main checkout before vs branch after interleaved back-to-back ×2 at load 2.0-2.1; parsed-body digest identical a344862b7fe2 across arms; the 34-id live-poll shape measured 2.81/2.65 ms → 2.37/2.28 ms in the same interleaved protocol) | the sidebar's 3 s poll renders through FastJsonResponse, skipping FastAPI's jsonable_encoder pass over the 41-row derived-state dict; M56 definition and healthy range introduced with this PR |
| 2026-09-04 | this PR | M57 /plans request median 3.63/3.54 ms → 2.77/2.74 ms, maxima 4.43/4.53 ms → 3.62/3.49 ms (collector verbatim, 15.4 KB worst plans corpus of session a9bb2346, live state read-only, main checkout before vs branch after interleaved back-to-back ×2 at load 2.0-2.1; parsed-body digest identical f0098c1aae15 across arms) | the plan panel's 3 s poll renders through FastJsonResponse, skipping FastAPI's jsonable_encoder pass over the 12-plan dict (the registry read itself is already the M27 memo at 10.6 µs); M57 definition and healthy range introduced with this PR |
