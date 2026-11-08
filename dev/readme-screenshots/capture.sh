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
#   2. write the fabricated session home with `mock-home generate` (240 sessions, view shape, seed 1);
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

# --inner runs as the served command of trial.sh: the trial environment has CHARLIEBOT_HOME pointed at
# the trial home, CHARLIEBOT_SERVE_URL at the trial server and CHARLIEBOT_SERVE_AUTH_HEADER at the
# Authorization header that server accepts.
if [[ ${1:-} == --inner ]]; then
  shift
  source /dev/stdin <<'INNER'
set -euo pipefail
SCRATCH=$1
REPO_ROOT=$2
CHROME=$3
BIN="$REPO_ROOT/rust/target/release"
URL=$CHARLIEBOT_SERVE_URL
AUTH=$CHARLIEBOT_SERVE_AUTH_HEADER
SESSIONS_DIR="$CHARLIEBOT_HOME/sessions"
OUT="$REPO_ROOT/docs/assets/readme"

[[ -n $URL && -n $AUTH && -n ${CHARLIEBOT_HOME:-} ]] || { printf 'inner: the trial environment is incomplete\n' >&2; exit 1; }
mkdir -p "$OUT"

# Deterministic picks out of the seeded home: one session carries the plan and the statuses the
# screenshots show, two more carry waiting items so the approval queue holds several rows.
mapfile -t sids < <(ls "$SESSIONS_DIR" | LC_ALL=C sort)
(( ${#sids[@]} >= 20 )) || { printf 'inner: the seed home holds only %s sessions\n' "${#sids[@]}" >&2; exit 1; }
MAIN=${sids[3]}
WAIT_B=${sids[8]}
WAIT_C=${sids[13]}

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
set_status() { # sid text
  local sid=$1 text=$2 body unread item tier answer payload
  body=$(jq -n --arg sid "$sid" --arg text "$text" '{session_id: $sid, text: $text}')
  unread=$(api POST /api/internal/web/status/unread "$body" | jq '.unread')
  local tier=t0
  while IFS= read -r item; do
    answer="The item's context reads as its description states, and the approval policy table tiers it.
tier: $tier rule: none"
    payload=$(jq -n --arg sid "$sid" --argjson item "$item" --arg backend "readme-demo" --arg answer "$answer" \
      '{session_id: $sid, item: $item, backend: $backend, answer: $answer}')
    api POST /api/internal/web/ask-check "$payload" >/dev/null
    tier=user-act
  done < <(jq -c '.[]' <<<"$unread")
  api POST /api/internal/web/status "$body" >/dev/null
}
set_status "$MAIN" "$(status_text \
  'Land the README refresh: rewritten entry docs, five new screenshots, and the delivery page.' \
  'The reviewer is checking the screenshots and the fact list against main.' \
  '[{"text": "Approve the delivery page publish", "context": "Publishing puts the page at a URL beyond your devices, so the step waits for you."}]')"
set_status "$WAIT_B" "$(status_text \
  'Prepare the release that carries the README refresh.' \
  'The release trial runs on the build host.' \
  '[{"text": "Confirm the release trial verdict", "context": "The trial log ends in the verdict; the release waits on it."}]')"
set_status "$WAIT_C" "$(status_text \
  'Keep the daily summary current while the README refresh lands.' \
  'Yesterday's summary went out with the digest attached.' \
  '[{"text": "Read the release trial log", "context": "The log names the stage verdicts; the summary quotes them."}]')"

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

# The plan page the plans panel shows: fabricated content in the plan genre.
PLAN_DIR="$SESSIONS_DIR/$MAIN/artifacts"
mkdir -p "$PLAN_DIR"
cat > "$PLAN_DIR/plan_01_v1.html" <<'PLAN'
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Demo plan: session archive policy</title>
</head>
<body>
<h1>Demo plan: session archive policy <span>PLAN</span></h1>
<p>Give closed sessions a single archive path so the sidebar stays a working set.</p>
<h2>1 Goal</h2>
<p>Archived sessions leave the sidebar but keep every run record, so an audit reads the full tree.</p>
<h2>2 Design</h2>
<ul>
  <li>An archive request writes a summary and the run ids it covers into the session record.</li>
  <li>The sidebar lists an archived session only in the archived filter.</li>
  <li>A subtree archive moves every descendant in one request.</li>
</ul>
<h2>3 Trade-offs</h2>
<ol>
  <li>Archive on run end or by request? By request: the reader decides when a thread is done, and the
      archived filter can undo it.</li>
</ol>
<h2>4 Acceptance</h2>
<ol>
  <li>An archived session answers its API reads with the same records as before.</li>
  <li>The sidebar's archived filter lists the session; the working filters do not.</li>
</ol>
</body>
</html>
PLAN
"$BIN/charliebot" plan present --session "$MAIN" --file artifacts/plan_01_v1.html \
  --title 'Demo plan: session archive policy' >/dev/null
"$BIN/charliebot" plan approve --session "$MAIN" >/dev/null

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
  const userDataDir = fs.mkdtempSync(path.join(process.env.TMPDIR || '/tmp', 'shoot-profile.'));
  const log = fs.openSync(path.join(process.env.SHOOT_LOG || '/tmp', 'chrome.log'), 'a');
  const child = spawn(binary, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
    '--disable-background-networking', '--disable-component-update', '--disable-sync',
    '--metrics-recording-only', '--password-store=basic', '--use-mock-keychain',
    \`--user-data-dir=\${userDataDir}\`, '--remote-debugging-pipe', 'about:blank',
  ], { detached: true, stdio: ['ignore', log, log, 'pipe', 'pipe'] });
  const exited = new Promise((resolve) => { child.once('exit', resolve); child.once('error', resolve); });
  const cdp = new Cdp(child.stdio[3], child.stdio[4]);
  const failed = new Promise((_, reject) => {
    child.once('exit', (code, signal) => reject(new Error(\`the browser exited (\${code ?? signal}) before its DevTools pipe answered\`)));
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
  await send('Network.setCookie', { name: 'charliebot_access_key', value: accessKey, url: baseUrl });
  return page;
}

async function evaluate(page, expression) {
  const result = await page.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (result.exceptionDetails) throw new Error(\`page script failed: \${result.exceptionDetails.text}\`);
  return result.result.value;
}

async function openUrl(page, url) {
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

SHOOT_LOG="$SCRATCH/chrome-driver.log" node "$SCRATCH/shoot.mjs" \
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

SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/readme-shots.XXXXXX")
cleanup() {
  if [[ $1 == 0 ]]; then
    rm -rf -- "$SCRATCH"
  else
    printf 'capture: failed; evidence stays in %s\n' "$SCRATCH"
  fi
}
trap 'status=$?; cleanup "$status"' EXIT

printf 'mock home: generating %s sessions (seed %s, view shape)\n' "$SESSIONS" "$SEED"
"$BIN/mock-home" generate "$SCRATCH/mock-home" --sessions "$SESSIONS" --seed "$SEED" --shape view

printf 'trial: starting the isolated server for commit %s\n' "$COMMIT"
"$REPO_ROOT/scripts/release/trial.sh" --serve --tree-root "$SCRATCH/tree" \
  --seed-home "$SCRATCH/mock-home" "$COMMIT" "$BIN/charliebot-server" \
  -- "$SCRIPT_PATH/capture.sh" --inner "$SCRATCH" "$REPO_ROOT" "$CHROME"

printf '\n'
for name in ui-chat-desktop ui-status-desktop ui-plans-desktop ui-approval-desktop ui-chat-mobile; do
  file="$REPO_ROOT/docs/assets/readme/$name.png"
  [[ -s $file ]] || { printf 'capture: %s is missing\n' "$file" >&2; exit 1; }
  printf 'wrote docs/assets/readme/%s.png  sha256 %s\n' "$name" "$(sha256sum "$file" | cut -d' ' -f1)"
done
