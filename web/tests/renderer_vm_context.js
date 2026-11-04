// ---------------------------------------------------------------------------
// The vm-context base shared by the harnesses that load markdown-renderer.js
// outside a browser. It holds a silent console and the hljs stub, plus a
// document whose only surface is querySelectorAll (the deferred highlight
// flush's element walk). A platform object stays for CHECKOUT A/B loads. An
// older checkout's renderer runs a load-time sidebar-link walk that reads
// platform when it finds anchors, and the querySelectorAll stub never returns
// any. withTimers adds the manual timer queue the deferred highlight flush runs
// on. setTimeout parks callbacks FIFO, __runTimers drains them, and __timerCount
// reports the queue length. requestAnimationFrame stays deliberately absent,
// because the renderer must schedule through setTimeout.
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
