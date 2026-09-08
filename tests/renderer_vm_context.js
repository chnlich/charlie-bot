// ---------------------------------------------------------------------------
// The vm-context base shared by the harnesses that load markdown-renderer.js
// outside a browser: a silent console, the hljs stub, a document whose only
// surface is querySelectorAll (the load-time call the renderer makes), and
// the platform object its sidebar-link walk reads when that walk finds
// anchors — which the querySelectorAll stub never returns. withTimers adds
// the manual timer queue the deferred highlight flush runs on: setTimeout
// parks callbacks FIFO, __runTimers drains them, __timerCount reports the
// queue length, and requestAnimationFrame stays deliberately absent — the
// renderer must schedule through setTimeout.
// ---------------------------------------------------------------------------
const { hljsStub } = require('./hljs_stub');

function buildRendererContext({ withTimers = false } = {}) {
  const timers = [];
  const context = {
    console: { error() {}, warn() {}, log() {} },
    hljs: hljsStub,
    document: { querySelectorAll: () => [] },
    platform: {},
  };
  if (withTimers) {
    context.performance = performance;
    context.setTimeout = (fn) => timers.push(fn);
    context.requestAnimationFrame = undefined;
    context.__timerCount = () => timers.length;
    context.__runTimers = () => { while (timers.length) timers.shift()(); };
  }
  return context;
}

module.exports = { buildRendererContext };
