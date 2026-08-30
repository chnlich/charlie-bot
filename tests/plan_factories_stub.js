// ---------------------------------------------------------------------------
// Plan snapshot factories for the node --test plan UI harnesses
// (plan_cards.test.js, plan_panel.test.js). Both suites render the same
// server plan-snapshot shape, so the shape lives here once: a field added on
// one side must land on this copy to reach both. makeVersion takes an
// optional verifyState override; every other field derives from v.
// ---------------------------------------------------------------------------
function makePlan(id, versions, opts) {
  const o = opts || {};
  return {
    id: id,
    title: o.title || 'Plan ' + id,
    versions: versions,
    takeoff: o.takeoff || null,
    closed: o.closed || null,
    state: o.state || 'in flight',
  };
}

function makeVersion(v, file, verifyState) {
  return {
    v: v,
    file: file || ('artifacts/plan_' + String(v).padStart(2, '0') + '.html'),
    created_at: '2026-07-20T00:00:00+00:00',
    trigger: v === 1 ? 'initial' : 'feedback',
    verify_thread: 'th_' + v,
    verify_state: verifyState || 'pending',
    base: null,
  };
}

module.exports = { makePlan, makeVersion };
