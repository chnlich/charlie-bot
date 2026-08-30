This run is the hourly latency-perf watch: measure the four latency metrics, compare them against
`docs/perf_baseline.md`, and land at most one fix as a pull request, or report a no-PR summary. It
works in the repo worktree: branch, fix, push, `gh pr create`, review the diff on the pull request,
then squash-merge it once the checks are green. `main` takes no direct pushes; every change lands
through the pull request, which stays the record of what was measured and why it changed. Before
anything else, read `skills/writing-style/genres/code.md` and follow it: comments carry constraints
only; provenance lives in blame.

Isolation rule: any validation that runs charliebot components — the config loader, cron task
loading, any worker spawn — runs against a scratch copy of the state directory with
`CHARLIEBOT_HOME` pointed at it, never against the live home. The live instance accepts only
read-only observation: the Step 1 collectors and the `GET /api/cron/tasks` readback. This run never
edits `~/.charliebot`, never restarts the server, and never touches CI.

Step 0: check for an open latency-perf pull request before anything else.
List open pull requests whose head branch starts with `latency-perf/`:

```bash
gh pr list --state open --json headRefName,number --jq '.[] | select(.headRefName | startswith("latency-perf/"))'
```

If any exists: produce no new PR. If that PR has no bot reminder comment in the last 7 days, leave
a one-line reminder comment that names this cron, then exit. Read this decision entirely from the
PR's comment history (`gh pr view <number> --json comments`); keep no state anywhere else. A run
that exits here reports the open pull request and the reminder decision, and does nothing else.

Step 1: measure the four metrics.
Run the four collectors below exactly as written; each prints one line for the run summary.
`docs/perf_baseline.md` carries the same commands in its collector listing — keep the two copies
identical when a change touches either. The M3 endpoint answers 401 without credentials; confirm
that status first, because the timing reads the middleware-and-framework floor of the server path.
The M4 projection reads only `type` and `timestamp` from chat events and `status` from thread
metadata — session content never enters this context or the summary. The M1 serve count includes
this run's own worker process, a constant bias present in every round: state it in the summary,
never read it as a regression.

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

M3 — API latency: the status check, then five credential-free timed requests:

```bash
curl -s -o /dev/null -w 'http_code=%{http_code} time_total=%{time_total}s\n' http://127.0.0.1:18498/api/sessions/status
for i in 1 2 3 4 5; do curl -s -o /dev/null -w '%{time_total}\n' http://127.0.0.1:18498/api/sessions/status; done | sort -n | awk '{a[NR]=$1} END {printf "median %.3f s, max %.3f s over %d requests\n", a[int((NR+1)/2)], a[NR], NR}'
```

M4 — turn durations and hung sessions:

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

Step 2: compare against the baseline and pick at most one topic.
Compare every measured number against the healthy ranges in `docs/perf_baseline.md`. Host-state
findings and LLM-provider findings always go to the run summary and never produce a PR. From the
remaining repo-fixable degradations, pick the single heaviest one. When the chosen topic's evidence
metric has no baseline entry, self-measure its before numbers first — under the same conditions the
fix will run in — and record the command, the numbers, and the load state in the Evidence section
before changing anything. If nothing repo-fixable survives, produce the no-PR run summary and exit.

Step 3: implement the fix.
Keep the pull request diff at 300 lines or fewer; when the budget runs out, stop and leave the
remainder to a later run. Reads must be targeted: this worker's backend has a hard 18k-token output
cap, so open only the files the fix needs and read only the relevant ranges.

Step 4: re-measure and open the pull request.
Re-run the collectors for every affected metric with the same commands and compare against the
Step 1 numbers. Branch `latency-perf/<slug>`, with a slug that self-describes the topic. At most
one PR per run. The body carries an `## Evidence` section quoting the before/after numbers plus
each command verbatim. When the topic introduced a metric that has no row in
`docs/perf_baseline.md`, add its definition row and a sampling-history row in the same PR, so later
rounds have a home for it. Run the `code-review` skill against the pull request with `--comment`
and act on its findings on the same branch; a skipped review is reported with the reason in the run
summary.

Step 5: land it.
Wait for the checks in this run:

```bash
gh pr checks <PR> --watch --fail-fast
```

Retry `--watch` a few times, several seconds apart, while it reports "no checks reported". With the
checks green, confirm the pull request still carries net content against a fresh `main`; a rival
run that already merged the same fix is invisible to Step 0, so this guard lives at merge time:

```bash
git fetch origin main && git diff origin/main...HEAD --stat
```

A non-empty diff merges:

```bash
gh pr merge --squash <PR>
```

An empty diff means the change already lives on `main`: close the pull request with the Step 6
abandonment comment. After a merge, fast-forward the repo's local main checkout (`git merge
--ff-only origin/main` in the main checkout) so sibling cron runs on this repo keep a fresh base
for the same guard.

Step 6: abandon path.
A red check earns one fix on the same branch: read it with `gh run view --log-failed`, fix, push,
and watch again, at most twice. Abandon the pull request when it stays red after the second fix,
or when the failure comes from outside this diff:

```bash
gh pr close <PR> --comment 'latency-perf-abandoned: <topic and reason>'
```

Name the topic in that comment. The next run reads the rejection ledger with the windowless query
— no `--limit`, because a limit once hid rejections — and skips every topic it returns:

```bash
gh search prs --repo "$(gh repo view --json nameWithOwner -q .nameWithOwner)" --state closed "latency-perf-abandoned"
```

Run summary: end every run that reaches measurement with a summary of five fixed sections:
1. the M1-M4 numbers as measured, with the serve-count bias note;
2. the comparison against the healthy ranges in `docs/perf_baseline.md`;
3. what was fixed and why, or why nothing was;
4. host-state and LLM-provider findings;
5. a recommendation for the next run.
