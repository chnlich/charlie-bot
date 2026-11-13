// ---------------------------------------------------------------------------
// Relative time formatting
// ---------------------------------------------------------------------------
function relativeTime(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  const dateStr = d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
  const timeStr = d.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit', hour12: true });
  return dateStr + ', ' + timeStr;
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
// loads before this one). Everything else here is a plain definition the page calls.
registerChatMathLoader(loadPageStyles, loadPageScripts);
