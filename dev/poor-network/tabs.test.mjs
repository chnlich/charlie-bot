// Tests of the switch step's row handling against a sidebar that keeps rows behind a group's "Show all". A bench that
// clicks a kept-back row directly makes a click no person can make, and stops once the page leaves the row out of its
// document; the step must go through the toggle, as a person does.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import vm from 'node:vm';

import { Tab } from './tabs.mjs';

// The toggle's listing fetch: the rows paint this long after its click.
const FETCH_MS = 30;

// A page whose sidebar holds one group with the key `group`. `painted` rows are on screen, `hidden` rows are in the
// document but not painted, and every other row of `full` is absent. A group that keeps rows back shows its Show all
// toggle, whose click paints every row of `full` once its fetch answers, as loadFullListing in
// web/static/js/sidebar/groups.js does. `clicks` records the row clicks and `toggles` the toggle clicks.
function sidebarPage({ group, painted, hidden = [], full }) {
  const visible = new Set(painted);
  let present = [...painted, ...hidden];
  const page = { clicks: [], toggles: 0 };
  const row = (id) => ({
    href: `https://front.test/?session=${id}`,
    checkVisibility: () => visible.has(id),
    click: () => page.clicks.push(id),
  });
  let expanded = false;
  const toggle = {
    dataset: { sgroupLimitToggleKey: group },
    getAttribute: (name) => (name === 'aria-expanded' ? String(expanded) : null),
    checkVisibility: () => true,
    click: () => {
      page.toggles += 1;
      setTimeout(() => {
        expanded = true;
        present = [...full];
        full.forEach((id) => visible.add(id));
      }, FETCH_MS);
    },
  };
  const keepsRowsBack = present.length < full.length || hidden.length > 0;
  const document = {
    querySelectorAll(selector) {
      if (selector === '#session-list a.session-row') return present.map(row);
      if (selector === '.session-group-limit-toggle') return keepsRowsBack ? [toggle] : [];
      throw new Error(`the page stub answers no selector ${selector}`);
    },
  };
  const context = vm.createContext({ document });
  page.tab = new Tab('visible', { evaluate: async (expression) => vm.runInContext(expression, context) });
  return page;
}

const PREVIEW = ['row-1', 'row-2', 'row-3', 'row-4', 'row-5'];
const FULL = [...PREVIEW, 'row-6', 'row-7', 'row-8'];

test('a row the page left out is reached through its group\'s Show all, then clicked', async () => {
  const page = sidebarPage({ group: '', painted: PREVIEW, full: FULL });
  await assert.rejects(page.tab.clickSession('row-7'), /no painted sidebar row for session row-7/);
  assert.equal(await page.tab.showRow({ id: 'row-7', group: '' }), true);
  await page.tab.clickSession('row-7');
  assert.deepEqual(page.clicks, ['row-7']);
  assert.equal(page.toggles, 1);
});

test('a hidden row is clicked only after the toggle paints it', async () => {
  const page = sidebarPage({ group: 'g', painted: PREVIEW, hidden: ['row-6', 'row-7', 'row-8'], full: FULL });
  await assert.rejects(page.tab.clickSession('row-7'), /no painted sidebar row/);
  assert.deepEqual(page.clicks, [], 'a person cannot click a row the page does not paint');
  assert.equal(await page.tab.showRow({ id: 'row-7', group: 'g' }), true);
  await page.tab.clickSession('row-7');
  assert.deepEqual(page.clicks, ['row-7']);
});

test('a painted row needs no toggle', async () => {
  const page = sidebarPage({ group: '', painted: PREVIEW, full: FULL });
  assert.equal(await page.tab.showRow({ id: 'row-2', group: '' }), false);
  assert.equal(page.toggles, 0);
});

test('a row that is neither painted nor behind its group\'s toggle fails the step', async () => {
  const page = sidebarPage({ group: '', painted: PREVIEW, full: FULL });
  await assert.rejects(page.tab.showRow({ id: 'row-9', group: 'other' }), /no sidebar row for session row-9/);
  assert.equal(page.toggles, 0);
});

test('the tab records a request the host\'s network change dropped, and leaves other failures out', () => {
  const tab = new Tab('open', { evaluate: async () => null });
  const request = (requestId, url) => ({
    method: 'Network.requestWillBeSent',
    params: { requestId, request: { url } },
  });
  const failure = (requestId, errorText) => ({ method: 'Network.loadingFailed', params: { requestId, errorText } });
  tab.handle(request('doc', 'https://front.test/?session=s'));
  tab.handle(request('poll', 'https://front.test/api/concurrency/now'));
  tab.handle(failure('doc', 'net::ERR_NETWORK_CHANGED'));
  tab.handle(failure('poll', 'net::ERR_CONNECTION_RESET'));
  assert.equal(tab.requests.get('doc').failed, 'net::ERR_NETWORK_CHANGED');
  assert.equal(tab.requests.get('poll').failed, 'net::ERR_CONNECTION_RESET');
  assert.equal(tab.networkChanged.length, 1, 'only the network-change error names the host\'s doing');
  assert.equal(tab.networkChanged[0].url, 'https://front.test/?session=s');
});
