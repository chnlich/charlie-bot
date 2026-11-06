// ---------------------------------------------------------------------------
// Page timers — every recurring poll/tick on the page goes through this
// registry so a hidden tab does no periodic work at all. Timers keep their
// registration while hidden; the interval itself is torn down and recreated on
// the next visibilitychange, and resume handlers run first so the page shows a
// fresh snapshot before the normal cadences restart.
// ---------------------------------------------------------------------------
const pageTimers = new Map();
const pageResumeHandlers = [];

function startPageTimer(name, fn, ms) {
  stopPageTimer(name);
  const timer = {fn, ms, id: null};
  pageTimers.set(name, timer);
  if (!document.hidden) timer.id = setInterval(fn, ms);
}

function stopPageTimer(name) {
  const timer = pageTimers.get(name);
  if (!timer) return;
  if (timer.id !== null) clearInterval(timer.id);
  pageTimers.delete(name);
}

function pageTimerRegistered(name) {
  return pageTimers.has(name);
}

// Runs on every hidden -> visible transition, before the intervals restart.
function onPageResume(fn) {
  pageResumeHandlers.push(fn);
}

// Work a hidden tab holds back: a request, or a reload. A visible tab runs fn at once. A hidden tab keeps fn under its
// key, and the next hidden -> visible transition runs each kept fn once, before the resume handlers. A later call under
// the same key replaces the kept fn.
const hiddenDeferred = new Map();

function deferWhileHidden(key, fn) {
  if (document.hidden) hiddenDeferred.set(key, fn);
  else fn();
}

function suspendPageTimers() {
  Array.from(pageTimers.values()).forEach(timer => {
    if (timer.id === null) return;
    clearInterval(timer.id);
    timer.id = null;
  });
}

function resumePageTimers() {
  const deferred = Array.from(hiddenDeferred.values());
  hiddenDeferred.clear();
  deferred.forEach(fn => fn());
  pageResumeHandlers.forEach(fn => fn());
  Array.from(pageTimers.values()).forEach(timer => {
    if (timer.id === null) timer.id = setInterval(timer.fn, timer.ms);
  });
}

document.addEventListener('visibilitychange', () => {
  if (document.hidden) suspendPageTimers();
  else resumePageTimers();
});
