// ---------------------------------------------------------------------------
// The reader's time label
// ---------------------------------------------------------------------------
// One label for the instants a reader acts on: "Oct 17, 2:43 PM", and "Oct 17, 2025, 2:43 PM" when the instant
// falls outside the current year. A home whose sessions span more than a year would otherwise print a session from
// last October and one from this March in the same shape, and neither would name its year. The year appears only
// where it tells two instants apart, so a home whose instants share one year reads exactly as it did.
function formatTimeLabel(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  const dateOptions = { month: 'short', day: 'numeric' };
  if (d.getFullYear() !== new Date().getFullYear()) dateOptions.year = 'numeric';
  const dateStr = d.toLocaleDateString('en-US', dateOptions);
  const timeStr = d.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit', hour12: true });
  return dateStr + ', ' + timeStr;
}

// The sidebar's rows (groups.js), its status panel and the cron list name their time through this label; the chat
// bubbles name it through chat/shared.js's formatBubbleTime. Both are this one function, so a row and a bubble
// state one instant alike.
function relativeTime(isoStr) {
  return formatTimeLabel(isoStr);
}

function updateRelativeTimes() {
  document.querySelectorAll('.session-time[data-time]').forEach(el => {
    el.textContent = relativeTime(el.dataset.time);
  });
  // The pending-triggers tray's remaining-time spans ride the same sweep, so
  // "(in 3h 52m)" stays current between the tray's own refetches.
  document.querySelectorAll('.tray-reltime[data-time]').forEach(el => {
    el.textContent = relativeFireIn(el.dataset.time);
  });
}

// ---------------------------------------------------------------------------
// Delayed-trigger time forms (the tray and the sidebar bell)
// ---------------------------------------------------------------------------

// "(in 3h 52m)" — the remaining time until an instant; "(due now)" once it has
// passed while the record is still pending. Hours do not roll into days: a
// 19h-away trigger reads "in 19h 17m".
function relativeFireIn(isoStr) {
  const ms = Date.parse(isoStr) - Date.now();
  if (!(ms > 0)) return '(due now)';
  const mins = Math.floor(ms / 60000);
  if (mins < 1) return '(in <1m)';
  if (mins < 60) return `(in ${mins}m)`;
  return `(in ${Math.floor(mins / 60)}h ${mins % 60}m)`;
}

// Local "HH:mm" (24h) — the bell title's and the tray's clock form.
function clockTimeHM(date) {
  return String(date.getHours()).padStart(2, '0') + ':' + String(date.getMinutes()).padStart(2, '0');
}

// Local "MM/DD HH:mm" — the tray's condition-line form.
function dateClockMDHM(date) {
  return String(date.getMonth() + 1).padStart(2, '0') + '/' + String(date.getDate()).padStart(2, '0')
    + ' ' + clockTimeHM(date);
}

// ---------------------------------------------------------------------------
// The page's row column form
// ---------------------------------------------------------------------------
// A page carries its sidebar rows as columns: one array per field, in row order, with null where a row leaves the
// field out. The columns declare under `$encoded` the cells that are not their field's value. A `uuid-b64` column
// holds the session ids' 16 bytes in base64url. A `delta` column holds the first value as it stands and every later
// value as its difference from the row before. The sidebar (groups.js) and the diff page's session picker both read
// that form, so one reader here expands it for both.
//
// A column row's `updated_at` is epoch minutes, while a listing route's row carries it as an ISO string. Both shapes
// reach a reader, so both go through sessionUpdatedMs.

// The base64url alphabet: a UUID session id travels as its 16 bytes in this alphabet.
const UUID_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';

// The bytes a base64url text carries.
function decodeBase64Url(text) {
  let buffer = 0;
  let bits = 0;
  const bytes = [];
  for (const character of text) {
    const index = UUID_ALPHABET.indexOf(character);
    if (index < 0) throw new Error('Encoded session id character ' + character);
    buffer = (buffer << 6) | index;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      bytes.push((buffer >> bits) & 0xff);
    }
  }
  return bytes;
}

// The session id 16 bytes spell in the canonical UUID form the store and the routes name it by.
function sessionIdOf(bytes) {
  if (bytes.length !== 16) throw new Error('Encoded session id of ' + bytes.length + ' bytes');
  const hex = bytes.map(byte => byte.toString(16).padStart(2, '0')).join('');
  return hex.slice(0, 8) + '-' + hex.slice(8, 12) + '-' + hex.slice(12, 16) + '-' +
    hex.slice(16, 20) + '-' + hex.slice(20);
}

// The session id one `uuid-b64` cell carries: 22 base64url characters that encode its 16 bytes, or a `~` id as
// its text.
function expandSessionId(cell) {
  if (cell[0] === '~') return cell.slice(1);
  if (cell.length !== 22) throw new Error('Encoded session id of ' + cell.length + ' characters');
  return sessionIdOf(decodeBase64Url(cell));
}

// The session ids one `uuid-b64` column carries. A string holds every id's 16 bytes in row order. An array holds one
// cell per row, which is how a home whose ids are not all canonical UUIDs travels.
function expandSessionIds(values, count) {
  if (Array.isArray(values)) return values.map(value => (value == null ? null : expandSessionId(value)));
  const bytes = decodeBase64Url(values);
  if (bytes.length !== count * 16) throw new Error('Encoded session id blob of ' + bytes.length + ' bytes');
  return Array.from({length: count}, (_, index) => sessionIdOf(bytes.slice(index * 16, (index + 1) * 16)));
}

function expandSessionColumns(columns) {
  const encoding = columns.$encoded || {};
  // Keys under `$` describe the listing rather than its rows: `$encoded` names the column encodings, and `$screen`
  // names the part of the sidebar the page carries.
  const fields = Object.keys(columns).filter(field => !field.startsWith('$'));
  const first = fields.length ? columns[fields[0]] : [];
  // The `id` column's string form carries 16 bytes per row, and base64url carries 3 bytes in 4 characters.
  const count = Array.isArray(first) ? first.length : Math.floor(first.length * 3 / 4 / 16);
  const rows = Array.from({length: count}, () => ({}));
  fields.forEach(field => {
    const values = columns[field];
    const kind = encoding[field];
    if (kind === 'uuid-b64') {
      expandSessionIds(values, count).forEach((id, index) => {
        if (id != null) rows[index][field] = id;
      });
      return;
    }
    let running = 0;
    for (let i = 0; i < count; i++) {
      let value = values[i];
      if (value == null) continue;
      if (kind === 'delta') {
        running += value;
        value = running;
      }
      rows[i][field] = value;
    }
  });
  return rows;
}

// The instant a row's updated_at names, in epoch milliseconds. A column row carries epoch minutes, a listing
// route's row an ISO string, and the session detail route an ISO string too.
function sessionUpdatedMs(session) {
  const value = session && session.updated_at;
  if (typeof value === 'number') return value * 60000;
  const parsed = Date.parse(value || '');
  return Number.isNaN(parsed) ? 0 : parsed;
}

// ---------------------------------------------------------------------------
// Feature scripts a page loads on first use
// ---------------------------------------------------------------------------
// A page carries most of its scripts in its document. A feature the reader may never open carries its scripts as
// files under /static instead, and loads them from the click that first needs it. The page's own bytes are then the
// price of the features the reader sees. One promise per key: a second click while the files travel joins the first
// load, and a failed load drops its entry so the next click tries again. The retry starts at the file that failed,
// because a file that already ran must not run twice: its registrations would be duplicates.
const pageScriptLoads = new Map();
const pageScriptProgress = new Map();

// A same-origin file takes the page's asset version, so a release that changes it also changes its URL. A file on
// another origin names its own version in its path (`xterm@5.3.0`), and a query the CDN does not serve would miss
// the entry its own pin already keyed, so it keeps the URL its module wrote.
function pageAssetUrl(path) {
  return /^https?:\/\//.test(path) ? path : path + '?v=' + encodeURIComponent(STATIC_ASSET_VERSION);
}

function loadPageScripts(key, paths) {
  if (!pageScriptLoads.has(key)) {
    const start = pageScriptProgress.get(key) || 0;
    const loadOne = (path, index) => new Promise((resolve, reject) => {
      const script = document.createElement('script');
      script.src = pageAssetUrl(path);
      script.onload = () => {
        pageScriptProgress.set(key, start + index + 1);
        resolve();
      };
      script.onerror = () => reject(new Error('the page script did not load: ' + path));
      document.head.appendChild(script);
    });
    const loading = paths.slice(start).reduce(
      (previous, path, index) => previous.then(() => loadOne(path, index)),
      Promise.resolve(),
    );
    pageScriptLoads.set(key, loading);
    loading.catch(() => pageScriptLoads.delete(key));
  }
  return pageScriptLoads.get(key);
}

// ---------------------------------------------------------------------------
// Feature stylesheets a page loads with the scripts they style
// ---------------------------------------------------------------------------
// A stylesheet in the page's document costs every reader the bytes of a feature they may never open, exactly as its
// scripts would. A feature whose scripts a page loads on first use therefore loads its stylesheet the same way. One
// promise per key, and a failed load drops its entry so the next need tries again. A stylesheet re-inserted after a
// failure costs nothing, because the rules it carries apply once, however many links name it.
const pageStyleLoads = new Map();

function loadPageStyles(key, paths) {
  if (!pageStyleLoads.has(key)) {
    const loadOne = (path) => new Promise((resolve, reject) => {
      const link = document.createElement('link');
      link.rel = 'stylesheet';
      link.href = pageAssetUrl(path);
      link.onload = () => resolve();
      link.onerror = () => reject(new Error('the page stylesheet did not load: ' + path));
      document.head.appendChild(link);
    });
    const loading = paths.reduce((previous, path) => previous.then(() => loadOne(path)), Promise.resolve());
    pageStyleLoads.set(key, loading);
    loading.catch(() => pageStyleLoads.delete(key));
  }
  return pageStyleLoads.get(key);
}

// The chat's math renderer takes these two by registration (markdown-renderer.js owns the walk and the files, and
// loads before this one on the chat page). A page that loads this file without that renderer, such as the diff page,
// registers nothing. Everything else here is a plain definition the page calls.
if (typeof registerChatMathLoader === 'function') registerChatMathLoader(loadPageStyles, loadPageScripts);
