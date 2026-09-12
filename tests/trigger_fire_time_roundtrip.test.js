const assert = require('node:assert/strict');
const test = require('node:test');

const {loadSidebarWorkersContext} = require('./sidebar_workers_context_stub');

// The list payload's trigger row ships fire_at as epoch-ms. The card paints it
// into data-fire-at, where DOM stringification makes it a bare numeric string,
// and the poll's updateTriggerStatus re-reads that string into the formatter.
// A stringified int is not a Date-parseable string, so the formatter must
// coerce the numeric form or the label renders NaN.
test('trigger fire-time label survives the data-fire-at string round trip', () => {
  const fireAtMs = Date.UTC(2026, 8, 12, 5, 47, 49, 631);
  const fireAtIso = new Date(fireAtMs).toISOString();
  const statusText = {textContent: '', dataset: {fireAt: String(fireAtMs)}};
  const icon = {attrs: {}, getAttribute(name) { return this.attrs[name]; }, setAttribute(name, value) { this.attrs[name] = value; }};
  const context = loadSidebarWorkersContext({
    document: {
      getElementById: (id) => {
        if (id === 'trigger-status-trg-1') return statusText;
        if (id === 'trigger-dot-trg-1') return icon;
        return null;
      },
      querySelectorAll: () => [],
    },
  });

  context.updateTriggerStatus('trg-1', 'pending');
  assert.ok(!statusText.textContent.includes('NaN'), statusText.textContent);
  assert.match(statusText.textContent, /^fires at /);

  statusText.dataset.fireAt = fireAtIso;
  context.updateTriggerStatus('trg-1', 'pending');
  const isoLabel = statusText.textContent;

  statusText.dataset.fireAt = String(fireAtMs);
  context.updateTriggerStatus('trg-1', 'pending');
  assert.equal(statusText.textContent, isoLabel);

  statusText.dataset.fireAt = String(fireAtMs);
  context.updateTriggerStatus('trg-1', 'fired');
  assert.match(statusText.textContent, /^fired at /);
  assert.ok(!statusText.textContent.includes('NaN'), statusText.textContent);
});
