#!/usr/bin/env bash
# Capture the README screenshots: five views of the web UI over fabricated demo data.
#
#   cd <repository root>
#   dev/remote-run/run.sh dev/readme-screenshots/capture.sh
#
# It runs on the build host through the remote runner: it builds the server there, and the headless
# browser lives there too. Steps, in order:
#
#   1. build rust/target/release/{charliebot,charliebot-server,mock-home} when they are missing;
#   2. write the fabricated session home with `mock-home generate` (240 sessions, view shape, seed 1, English
#      prose), so every seeded sentence, title and label reads in one language in the public README;
#   3. start the isolated server through `scripts/release/trial.sh --serve` — the trial's own tree, its
#      own home and a port from the trial range; the live server's port and home stay untouched — and
#      hand the served command to this script's --inner mode, which fabricates the plan pages, waiting
#      items, digest entries and session statuses through the trial server's own API and CLI, then
#      photographs the five views with the node driver this script writes into its scratch;
#   4. overwrite docs/assets/readme/ui-chat-desktop.png, ui-status-desktop.png, ui-plans-desktop.png,
#      ui-approval-desktop.png and ui-chat-mobile.png, and print each file's sha256.
#
# Viewports: desktop 2100x1560 (the size of the screenshots this replaces); phone 390x844.
#
# The browser binary resolves in this order: $README_CAPTURE_CHROME, then google-chrome,
# chromium-browser, chromium. A candidate counts only when `--version` prints a version line.
#
# The DevTools-over-pipe driver duplicates the connection class of dev/poor-network/chrome.mjs
# without its shaping proxy, which a plain http trial server does not need.
#
# Failure: the step or the view that failed is printed, the exit code is nonzero, and the scratch
# directory is kept and its path printed for the evidence. A successful run removes the scratch.
set -euo pipefail

SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_PATH/../.." && pwd)"
readonly SCRIPT_PATH REPO_ROOT

readonly SESSIONS=240
readonly SEED=1
readonly DESKTOP_WIDTH=2100
readonly DESKTOP_HEIGHT=1560
readonly PHONE_WIDTH=390
readonly PHONE_HEIGHT=844

resolve_chrome() {
  local candidates=()
  if [[ -n ${README_CAPTURE_CHROME:-} ]]; then
    candidates+=("$README_CAPTURE_CHROME")
  else
    candidates+=(google-chrome chromium-browser chromium)
  fi
  local candidate resolved
  for candidate in "${candidates[@]}"; do
    resolved="$(command -v -- "$candidate" 2>/dev/null || true)"
    [[ -n $resolved ]] || continue
    # A launcher stub prints an install hint instead of a version, so the version line is the probe.
    if "$resolved" --version >/dev/null 2>&1; then
      printf '%s\n' "$resolved"
      return 0
    fi
  done
  return 1
}

build_binaries() {
  local bin="$REPO_ROOT/rust/target/release"
  local missing=0 name
  for name in charliebot charliebot-server mock-home; do
    [[ -x "$bin/$name" ]] || missing=1
  done
  if (( missing )); then
    printf 'build: compiling charliebot-server, charliebot and mock-home\n'
    (cd "$REPO_ROOT/rust" && cargo build --release --locked \
      -p charliebot-server -p charliebot-cli -p charliebot-mock-home)
  fi
  for name in charliebot charliebot-server mock-home; do
    [[ -x "$bin/$name" ]] || { printf 'capture: the build produced no %s\n' "$name" >&2; return 1; }
  done
}

# Rewrite one JSON file through a jq program. jq writes a scratch file first, so a failed program stops the
# script before the file changes; the copy back keeps the file's inode and mode. -a writes non-ASCII text as
# \u escapes, so the rewritten files are ASCII.
rewrite_json() { # file jq-args... program
  local file=$1 tmp
  shift
  tmp=$(mktemp)
  jq -a "$@" "$file" > "$tmp"
  cat -- "$tmp" > "$file"
  rm -f -- "$tmp"
}

# --inner runs as the served command of trial.sh: the trial environment has CHARLIEBOT_HOME pointed at
# the trial home, CHARLIEBOT_SERVE_URL at the trial server and CHARLIEBOT_SERVE_AUTH_HEADER at the
# Authorization header that server accepts.
if [[ ${1:-} == --inner ]]; then
  shift
  source /dev/stdin <<'INNER'
set -euo pipefail
# REPO_ROOT is already resolved above from this script's own path; the served command re-enters it.
SCRATCH=$1
CHROME=$3
BIN="$REPO_ROOT/rust/target/release"
URL=$CHARLIEBOT_SERVE_URL
AUTH=$CHARLIEBOT_SERVE_AUTH_HEADER
SESSIONS_DIR="$CHARLIEBOT_HOME/sessions"
OUT="$REPO_ROOT/docs/assets/readme"

[[ -n $URL && -n $AUTH && -n ${CHARLIEBOT_HOME:-} ]] || { printf 'inner: the trial environment is incomplete\n' >&2; exit 1; }
mkdir -p "$OUT"

# The session the screenshots open is the one with the largest chat record, so the chat view is full;
# two more carry waiting items so the approval queue holds several rows.
MAIN=$(find "$SESSIONS_DIR" -name chat_events.jsonl -printf '%s %h\n' \
  | awk '$1 > max { max = $1; dir = $2 } END { print dir }' | xargs dirname | xargs basename)
mapfile -t sids < <(ls "$SESSIONS_DIR" | LC_ALL=C sort)
(( ${#sids[@]} >= 20 )) || { printf 'inner: the seed home holds only %s sessions\n' "${#sids[@]}" >&2; exit 1; }
WAIT_B=${sids[40]}
WAIT_C=${sids[80]}
printf 'inner: main session %s\n' "$MAIN"

api() { # method path body
  curl -fsS -X "$1" -H "$AUTH" -H 'Content-Type: application/json' ${3:+-d "$3"} "$URL$2"
}

status_text() { # goal now waiting_json
  jq -n --arg goal "$1" --arg now "$2" --argjson waiting "$3" \
    '{goal: $goal, now: $now, waiting: $waiting}'
}

# Store one session's status. Every Waiting on you item needs a recorded cold read before the server
# stores it, so each unread item first gets one ask-check whose answer ends with the tier line the
# approval policy parser reads.
set_status() { # sid text tier
  local sid=$1 text=$2 tier=$3 body unread item answer payload
  body=$(jq -n --arg sid "$sid" --arg text "$text" '{session_id: $sid, text: $text}')
  unread=$(api POST /api/internal/web/status/unread "$body" | jq '.unread')
  while IFS= read -r item; do
    answer="The item's context reads as its description states, and the approval policy table tiers it.
tier: $tier rule: none"
    payload=$(jq -n --arg sid "$sid" --argjson item "$item" --arg backend "readme-demo" --arg answer "$answer" \
      '{session_id: $sid, item: $item, backend: $backend, answer: $answer}')
    api POST /api/internal/web/ask-check "$payload" >/dev/null
  done < <(jq -c '.[]' <<<"$unread")
  api POST /api/internal/web/status "$body" >/dev/null
}
set_status "$MAIN" "$(status_text \
  'Land the README refresh: rewritten entry docs, five new screenshots, and the delivery page.' \
  'The reviewer is checking the screenshots and the fact list against main.' \
  '[{"text": "Approve the delivery page publish", "context": "Publishing puts the page at a URL beyond your devices, so the step waits for you."}]')" t0
set_status "$WAIT_B" "$(status_text \
  'Prepare the release that carries the README refresh.' \
  'The release trial runs on the build host.' \
  '[{"text": "Confirm the release trial verdict", "context": "The trial log ends in the verdict; the release waits on it."}]')" t0
set_status "$WAIT_C" "$(status_text \
  'Keep the daily summary current while the README refresh lands.' \
  "Yesterday's summary went out with the digest attached." \
  '[{"text": "Read the release trial log", "context": "The log names the stage verdicts; the summary quotes them."}]')" user-act

# Freeze the fabricated timestamps: the Status view renders the stored updated_at and the item
# ages verbatim, so run-time stamps would age the view while it sits in the repository. The store
# reads session metadata from disk on each request; the web_status chat event's copy of the
# payload is patched too, though the page can also take that value from the server's memory, in
# which case the "updated" clock shows the run's time — the one glyph that varies between runs.
FIXED_STAMP=2026-11-08T12:00:00Z
# The item age the Status view shows is now minus since in whole hours, so the since stamp rides
# one day behind the run: the age renders as 1d whatever time the run happens at.
DAY_OLD_STAMP=$(date -u -d "@$(( $(date -u +%s) - 86400 ))" +%Y-%m-%dT%H:%M:%SZ)
# freeze stamps one web_status payload: updated_at, and since and since_exact on each waiting item. A payload
# with no waiting list gets only updated_at.
FREEZE_PAYLOAD='def freeze: .updated_at = $stamp
  | if (.waiting | type) == "array" then .waiting |= map(.since = $day_old | .since_exact = true) else . end;'
# Two programs apply freeze: one to the web_status of a metadata file, one to each web_status chat event.
METADATA_PROGRAM="$FREEZE_PAYLOAD"'
  if (.web_status | type) == "object" then .web_status |= freeze else . end'
EVENT_PROGRAM="$FREEZE_PAYLOAD"'
  if .type == "web_status" and (.web_status | type) == "object" then .web_status |= freeze else . end'
for sid in "$MAIN" "$WAIT_B" "$WAIT_C"; do
  rewrite_json "$SESSIONS_DIR/$sid/metadata.json" --arg stamp "$FIXED_STAMP" --arg day_old "$DAY_OLD_STAMP" \
    "$METADATA_PROGRAM"
  # The Status view reads its updated_at and the item ages from the web_status
  # chat event when it is newer than the stored status, so the event's copy of
  # the payload takes the same stamps.
  rewrite_json "$SESSIONS_DIR/$sid/data/chat_events.jsonl" -c --arg stamp "$FIXED_STAMP" \
    --arg day_old "$DAY_OLD_STAMP" "$EVENT_PROGRAM"
done

digest() { # policy step undo scope_check
  local args=(digest add --session "$MAIN" --policy "$1" --step "$2" --undo "$3")
  [[ -n ${4:-} ]] && args+=(--scope-check "$4")
  "$BIN/charliebot" "${args[@]}" >/dev/null
}
digest t2 'Landed the README refresh on main' 'Revert the delivery commit on main'
digest release-restart 'Released the server onto the commit that carries the README refresh' \
  'Switch the server back to the previous release' \
  'The scope the release script owns: install the release, switch the server, restart'
digest t2 'Sent the screenshot review request from your account' \
  'The message cannot be recalled; post the correction in the same thread'

# The plan page the plans panel shows, and its registry row. The plan CLI's registration runs the
# plan genre's page-height assertion through the server, whose trial config carries no
# headless_chrome_bin, so this writes the session's plans.json and the artifact directly — the home
# files the CLI's own registration would have written.
PLAN_DIR="$SESSIONS_DIR/$MAIN/artifacts"
mkdir -p "$PLAN_DIR"
cat > "$PLAN_DIR/plan_01_v1.html" <<'PLAN'
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>CharlieBot Plan</title>
<style>
  :root{--bg:#f3f5f7;--panel:#ffffff;--panel2:#eef1f5;--bd:#d9dfe6;--tx:#1b2430;--mut:#5b6774;--acc:#1163c0;--grn:#187f37;--amb:#9a6700;--mono:ui-monospace,Menlo,Consolas,monospace}
  *{box-sizing:border-box}
  body{margin:0;background:var(--bg);color:var(--tx);font:14px/1.55 -apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;padding:24px}
  .wrap{max-width:1000px;margin:0 auto}
  h1{font-size:21px;margin:0 0 4px}
  .status{display:inline-block;font-size:10.5px;font-weight:700;letter-spacing:.6px;background:var(--grn);color:#fff;border-radius:5px;padding:2px 7px;vertical-align:middle;margin-left:8px}
  .sub{color:var(--mut);margin:0 0 18px;font-size:13px}
  .meta{display:flex;flex-wrap:wrap;gap:8px;margin:0 0 16px}
  .mtag{font-size:11.5px;font-family:var(--mono);background:var(--panel2);border:1px solid var(--bd);border-radius:6px;padding:3px 9px}
  h2{font-size:15px;margin:26px 0 10px;padding-bottom:6px;border-bottom:1px solid var(--bd)}
  h2 .n{display:inline-flex;width:22px;height:22px;border-radius:6px;background:var(--acc);color:#fff;font-size:12px;font-weight:700;align-items:center;justify-content:center;margin-right:8px;vertical-align:-4px}
  p{margin:7px 0}
  ul,ol{margin:6px 0 0;padding-left:18px}
  li{margin:3px 0}
  .src{font-size:11px;color:#66707b}
  code{font-family:var(--mono);background:var(--panel2);border:1px solid var(--bd);border-radius:4px;padding:1px 5px;font-size:12.5px;color:#7a4b00}
  pre{background:#f6f8fa;border:1px solid var(--bd);border-radius:8px;padding:11px 13px;overflow:auto;font-family:var(--mono);font-size:12px;margin:9px 0 0}
  .fork{background:var(--panel);border:1px solid var(--bd);border-left:3px solid var(--amb);border-radius:8px;padding:12px 15px;margin:12px 0}
  .fork .q{font-weight:600;margin:0 0 4px}
  .fork .fn{display:inline-flex;width:20px;height:20px;border-radius:6px;background:var(--amb);color:#fff;font-size:11.5px;font-weight:700;align-items:center;justify-content:center;margin-right:9px;vertical-align:-3px}
  .fork .rec{margin:4px 0 0;font-size:12.5px}
  .fork .rec b{color:var(--grn)}
  .fork .trade{margin:3px 0 0;font-size:12.5px;color:var(--mut)}
  .fork .resolved{margin:8px 0 0;padding-top:8px;border-top:1px dashed var(--bd);font-size:12.5px;font-weight:600;color:#187f37}
  .foot{margin-top:22px;padding:13px 15px;background:linear-gradient(90deg,rgba(188,140,255,.10),transparent);border:1px solid var(--bd);border-radius:9px;color:var(--mut);font-size:12.5px}
</style>
</head>
<body>
<div class="wrap">
  <h1>Demo plan: session archive policy<span class="status">APPROVED</span></h1>
  <p class="sub">Give closed sessions one archive path so the sidebar stays a working set, without losing a run record.</p>
  <div class="meta">
    <span class="mtag">repo &middot; workspace/synthetic</span>
    <span class="mtag">base &middot; main</span>
    <span class="mtag">v1 &middot; initial</span>
  </div>
  <h2><span class="n">1</span> Problem / Goal</h2>
  <p>Closed sessions keep their place in the sidebar, so the working set grows past what a person scans.
     The archive path has to keep every run record readable and undoable.</p>
  <h2><span class="n">2</span> Context</h2>
  <p>The sidebar reads its rows from the session records; the archived filter exists but nothing moves a
     session into it today. <span class="src">web/static/js/sidebar/archived.js</span></p>
  <h2><span class="n">3</span> High Level Solution</h2>
  <p>An archive request writes a summary and the covered run ids into the session record, and the record's
     status moves to archived. The sidebar lists the session only in the archived filter; the API reads
     answer unchanged.</p>
  <h2><span class="n">4</span> Detailed Design</h2>
  <ul>
    <li>The request carries the summary, the run ids and a request id that binds the retry.</li>
    <li>A subtree archive moves every descendant in one request.</li>
    <li>Opening an archived session serves the full history, runs included.</li>
  </ul>
  <h2><span class="n">5</span> Trade-offs</h2>
  <div class="fork">
    <p class="q"><span class="fn">1</span> Archive on run end or by request?</p>
    <p class="rec"><b>By request:</b> the reader decides when a thread is done, and the archived filter undoes it.</p>
    <p class="trade">Cost: a session nobody archives lingers; the daily digest names the candidates instead.</p>
    <p class="resolved">Resolved: by request, with the digest listing archive candidates.</p>
  </div>
  <h2><span class="n">6</span> Other Details</h2>
  <ul>
    <li>The archived filter keeps its own page of rows, so the working filters stay fast.</li>
  </ul>
  <div class="foot"><p>Approved after one verify round and the cold read. The plan registry holds this
     version's record; the artifact viewer shows it with its switchers.</p></div>
</div>
</body>
</html>
PLAN
jq -n --arg created "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg approved "$(date -u +%Y-%m-%dT%H:%M:%SZ)"   '{plans: [{
      id: 1,
      title: "Demo plan: session archive policy",
      takeoff: ({v: 1, at: $approved}),
      closed: null,
      versions: [{v: 1, file: "artifacts/plan_01_v1.html", created_at: $created, trigger: "initial", base: null, note: null}]
  }]}' > "$SESSIONS_DIR/$MAIN/plans.json"

cat > "$SCRATCH/shoot.mjs" <<DRIVER
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const [baseUrl, accessKey, outDir, mainSession, chrome] = process.argv.slice(2);
const DESKTOP = { width: $DESKTOP_WIDTH, height: $DESKTOP_HEIGHT, mobile: false };
const PHONE = { width: $PHONE_WIDTH, height: $PHONE_HEIGHT, mobile: true };

// The DevTools-over-pipe connection, duplicated from dev/poor-network/chrome.mjs without the proxy
// coupling: one JSON message per NUL-terminated frame on fds 3 and 4.
class Cdp {
  constructor(writable, readable) {
    this.nextId = 1;
    this.pending = new Map();
    this.buffer = '';
    this.writable = writable;
    readable.setEncoding('utf8');
    readable.on('data', (chunk) => {
      this.buffer += chunk;
      for (;;) {
        const end = this.buffer.indexOf('\0');
        if (end < 0) return;
        const message = JSON.parse(this.buffer.slice(0, end));
        this.buffer = this.buffer.slice(end + 1);
        if (message.id === undefined) continue;
        const waiter = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (message.error) waiter.reject(new Error(\`\${waiter.method}: \${message.error.message}\`));
        else waiter.resolve(message.result ?? {});
      }
    });
  }
  send(method, params = {}, sessionId = undefined) {
    const id = this.nextId++;
    const message = sessionId ? { id, method, params, sessionId } : { id, method, params };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject, method });
      this.writable.write(\`\${JSON.stringify(message)}\0\`);
    });
  }
  close() {
    for (const waiter of this.pending.values()) waiter.reject(new Error(\`\${waiter.method}: the browser closed\`));
    this.pending.clear();
    this.writable.destroy();
  }
}

async function launchBrowser(binary) {
  // Chrome places its process-singleton socket under TMPDIR, and the trial points TMPDIR into its own
  // tree, which is too long for the socket's 108-byte path. The caller's short TMPDIR comes back in
  // README_CAPTURE_TMPDIR, so the profile goes there and the browser takes it as its TMPDIR.
  const shortTmp = process.env.README_CAPTURE_TMPDIR || process.env.TMPDIR || '/tmp';
  const userDataDir = fs.mkdtempSync(path.join(shortTmp, 'shoot-profile.'));
  const logPath = path.join(process.env.SHOOT_LOG || shortTmp, 'chrome.log');
  const log = fs.openSync(logPath, 'a');
  const child = spawn(binary, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
    '--disable-background-networking', '--disable-component-update', '--disable-sync',
    '--metrics-recording-only', '--password-store=basic', '--use-mock-keychain',
    \`--user-data-dir=\${userDataDir}\`, '--remote-debugging-pipe', 'about:blank',
  ], {
    detached: true,
    stdio: ['ignore', log, log, 'pipe', 'pipe'],
    env: { ...process.env, TMPDIR: shortTmp, TMP: shortTmp, TEMP: shortTmp },
  });
  // A browser that closes the pipe still has to reach the exit race below, which reports its log; the
  // socket's own error would otherwise throw before that message is printed.
  child.stdio[4].on('error', () => {});
  const exited = new Promise((resolve) => { child.once('exit', resolve); child.once('error', resolve); });
  const cdp = new Cdp(child.stdio[3], child.stdio[4]);
  const failed = new Promise((_, reject) => {
    child.once('exit', (code, signal) => {
      let tail = '';
      try { tail = fs.readFileSync(logPath, 'utf8').slice(-3000); } catch (error) { tail = \`(no browser log: \${error.message})\`; }
      reject(new Error(\`the browser exited (\${code ?? signal}) before its DevTools pipe answered\n\${tail}\`));
    });
  });
  failed.catch(() => {});
  await Promise.race([cdp.send('Browser.getVersion'), failed]);
  return {
    cdp, child, exited,
    async close() {
      try { process.kill(-child.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      await exited;
      cdp.close();
    },
  };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function openPage(browser) {
  const { targetId } = await browser.cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await browser.cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const page = { cdp: browser.cdp, sessionId, targetId };
  const send = (method, params = {}) => browser.cdp.send(method, params, sessionId);
  page.send = send;
  await send('Page.enable');
  await send('Network.enable');
  await send('Runtime.enable');
  await send('Emulation.setDeviceMetricsOverride', { ...DESKTOP, deviceScaleFactor: 1 });
  return page;
}

async function evaluate(page, expression) {
  const result = await page.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (result.exceptionDetails) throw new Error(\`page script failed: \${result.exceptionDetails.text}\`);
  return result.result.value;
}

async function openUrl(page, url) {
  // The page's initAuth reads the access key from localStorage and mirrors it into a cookie, so both
  // go in the way the page itself writes them; every navigation after the first then carries the key.
  await page.send('Page.navigate', { url: baseUrl + '/' });
  await sleep(500);
  await evaluate(page, \`localStorage.setItem('charliebot_access_key', \${JSON.stringify(accessKey)});
    document.cookie = 'charliebot_access_key=' + \${JSON.stringify(accessKey)} + '; path=/; SameSite=Strict';\`);
  const auth = await evaluate(page,
    "fetch('/api/auth/status').then(r => r.text().then(t => r.status + ' ' + t)).catch(e => 'ERR ' + e)");
  console.log(\`auth: \${auth}, cookie bytes: \${(await evaluate(page, 'document.cookie')).length}\`);
  await page.send('Page.navigate', { url });
  await sleep(2500);
}

async function waitTruthy(page, expression, label, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await evaluate(page, expression)) return;
    await sleep(250);
  }
  throw new Error(\`the page never reached: \${label}\`);
}

async function shoot(browser, name, prepare) {
  const page = await openPage(browser);
  try {
    await prepare(page);
    const { data } = await page.send('Page.captureScreenshot', { format: 'png' });
    const file = path.join(outDir, name);
    fs.writeFileSync(file, Buffer.from(data, 'base64'));
    console.log(\`wrote \${file}\`);
  } finally {
    await browser.cdp.send('Target.closeTarget', { targetId: page.targetId });
  }
}

const chatUrl = \`\${baseUrl}/?session=\${mainSession}\`;
const CHAT_READY = "document.querySelector('#messages') && document.querySelectorAll('#messages *').length > 3";
const EVERYTHING = "document.querySelector('[data-chat-view-option=everything]').click()";

const views = [
  ['ui-chat-desktop.png', async (page) => {
    await openUrl(page, chatUrl);
    await waitTruthy(page, CHAT_READY, 'the chat column');
    await evaluate(page, EVERYTHING);
    await sleep(1200);
  }],
  ['ui-status-desktop.png', async (page) => {
    await openUrl(page, chatUrl);
    await waitTruthy(page, CHAT_READY, 'the chat column');
    await evaluate(page, "document.querySelector('[data-chat-view-option=status]').click()");
    await waitTruthy(page, "!document.querySelector('#status-panel').classList.contains('hidden') && document.querySelector('#status-panel').innerText.trim().length > 20", 'the status panel');
    await sleep(600);
  }],
  ['ui-plans-desktop.png', async (page) => {
    await openUrl(page, chatUrl);
    await waitTruthy(page, CHAT_READY, 'the chat column');
    await evaluate(page, "document.querySelector('#btn-chat-plans').click()");
    await waitTruthy(page, "(() => { const el = document.querySelector('#tab-plans'); return el && !el.classList.contains('hidden') && el.innerText.trim().length > 40; })()", 'the plans panel');
    await sleep(800);
  }],
  ['ui-approval-desktop.png', async (page) => {
    await openUrl(page, \`\${baseUrl}/approval\`);
    await waitTruthy(page, "document.body && document.body.innerText.includes('Digest')", 'the approval page body');
    // The day's digest starts folded, and the README caption names its entries and their undos, so the
    // shot opens the fold when the page carries one.
    await evaluate(page, "(() => { const h = document.querySelector('h2.digest-heading'); if (h && h.querySelector('.hint[role=button]')) h.click(); })()");
    await sleep(800);
  }],
  ['ui-chat-mobile.png', async (page) => {
    await page.send('Emulation.setDeviceMetricsOverride', { ...PHONE, deviceScaleFactor: 1 });
    await openUrl(page, chatUrl);
    await waitTruthy(page, CHAT_READY, 'the chat column');
    await evaluate(page, EVERYTHING);
    await sleep(1200);
  }],
];

const browser = await launchBrowser(chrome);
try {
  for (const [name, prepare] of views) {
    try {
      await shoot(browser, name, prepare);
    } catch (error) {
      throw new Error(\`view \${name}: \${error.message}\`);
    }
  }
} finally {
  await browser.close();
}
DRIVER

SHOOT_LOG="$SCRATCH" node "$SCRATCH/shoot.mjs" \
  "$URL" "$(printf '%s' "$AUTH" | sed 's/^Authorization: Bearer //')" "$OUT" "$MAIN" "$CHROME"

printf 'inner: fabrication and capture finished\n'
INNER
  exit $?
fi

CHROME=$(resolve_chrome) || { printf 'capture: no browser binary; set README_CAPTURE_CHROME\n' >&2; exit 1; }
printf 'browser: %s (%s)\n' "$CHROME" "$("$CHROME" --version 2>/dev/null | head -1)"

build_binaries
BIN="$REPO_ROOT/rust/target/release"
COMMIT=$(git -C "$REPO_ROOT" rev-parse HEAD)
[[ $COMMIT =~ ^[0-9a-f]{40}$ ]] || { printf 'capture: %s is not a full commit sha\n' "$COMMIT" >&2; exit 1; }

# The trial reads the operator's config through CHARLIEBOT_HOME, so on a build host with no profile
# home this script writes a bench-owned one: a stand-in endpoint the served command never calls.
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/readme-shots.XXXXXX")
mkdir -p "$SCRATCH/live-home"
cat > "$SCRATCH/live-home/config.yaml" <<'YAML'
backends:
  options:
    - id: readme-capture-endpoint
      label: Readme capture endpoint
      type: charlie-code
      model: capture/stub
      api_base: http://127.0.0.1:9/v1
YAML
cleanup() {
  if [[ $1 == 0 ]]; then
    rm -rf -- "$SCRATCH"
  else
    printf 'capture: failed; evidence stays in %s\n' "$SCRATCH"
  fi
}
trap 'status=$?; cleanup "$status"' EXIT

printf 'mock home: generating %s sessions (seed %s, view shape, English prose)\n' "$SESSIONS" "$SEED"
"$BIN/mock-home" generate "$SCRATCH/mock-home" --sessions "$SESSIONS" --seed "$SEED" --shape view \
  --language english

printf 'trial: starting the isolated server for commit %s\n' "$COMMIT"
mkdir -p "$SCRATCH/tree"
# The server walks the mock home at boot inside the caller's scope, as the browser bench's trial does.
README_CAPTURE_TMPDIR="${TMPDIR:-/tmp}" CHARLIEBOT_HOME="$SCRATCH/live-home" \
  systemd-run --user --scope -p MemoryMax=16G -p MemorySwapMax=0 --unit="readme-shots-$$" \
  "$REPO_ROOT/scripts/release/trial.sh" --serve --in-caller-scope --tree-root "$SCRATCH/tree" \
  --seed-home "$SCRATCH/mock-home" "$COMMIT" "$BIN/charliebot-server" \
  -- "$SCRIPT_PATH/capture.sh" --inner "$SCRATCH" "$REPO_ROOT" "$CHROME"

printf '\n'
for name in ui-chat-desktop ui-status-desktop ui-plans-desktop ui-approval-desktop ui-chat-mobile; do
  file="$REPO_ROOT/docs/assets/readme/$name.png"
  [[ -s $file ]] || { printf 'capture: %s is missing\n' "$file" >&2; exit 1; }
  printf 'wrote docs/assets/readme/%s.png  sha256 %s\n' "$name" "$(sha256sum "$file" | cut -d' ' -f1)"
done
