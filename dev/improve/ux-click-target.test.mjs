// Tests of the improve-ux click helper. The helper's page functions run in a vm context whose `document` is a fake page:
// a tree of elements with boxes, a hit test at a point, and the page's reads counted, so a case can change the page
// between two reads. The fake send answers Runtime.evaluate by running the expression in that context, as CDP does, and
// it records each Input event. A test file's name ends in .test.mjs, so the bench-lock naming check reads it as a test.

import assert from 'node:assert/strict';
import vm from 'node:vm';
import { test } from 'node:test';

import { ClickRefused, clickTarget } from './ux-click-target.mjs';

const BOX = { left: 10, top: 10, width: 80, height: 24 };

class FakeElement {
  constructor(tag, { matches = null, id = '', classes = [], rect = BOX, children = [] } = {}) {
    this.tagName = tag.toUpperCase();
    this.matches = matches;
    this.id = id;
    this.classList = classes;
    this.rect = rect;
    this.children = children;
    this.scrolls = 0;
  }

  getBoundingClientRect() {
    return { ...this.rect };
  }

  scrollIntoView() {
    this.scrolls += 1;
  }

  contains(node) {
    return node === this || this.children.some((child) => child.contains(node));
  }
}

function* walk(nodes) {
  for (const node of nodes) {
    yield node;
    yield* walk(node.children);
  }
}

function inside(rect, x, y) {
  return x >= rect.left && x < rect.left + rect.width && y >= rect.top && y < rect.top + rect.height;
}

// The element a point hits: the topmost root, then the topmost child that holds the point, down to the deepest.
function deepestAt(node, x, y) {
  if (!inside(node.getBoundingClientRect(), x, y)) return null;
  for (const child of [...node.children].reverse()) {
    const hit = deepestAt(child, x, y);
    if (hit !== null) return hit;
  }
  return node;
}

// The page's document. ROOTS are in paint order, so a later root covers an earlier one.
class FakePage {
  constructor(roots = []) {
    this.roots = roots;
    this.reads = 0;
    this.onRead = null;
    this.invalidSelectors = new Set();
  }

  querySelector(selector) {
    if (this.invalidSelectors.has(selector)) {
      throw new SyntaxError(`'${selector}' is not a valid selector`);
    }
    for (const node of walk(this.roots)) {
      if (node.matches === selector) return node;
    }
    return null;
  }

  elementFromPoint(x, y) {
    for (const root of [...this.roots].reverse()) {
      const hit = deepestAt(root, x, y);
      if (hit !== null) return hit;
    }
    return null;
  }
}

// The CDP send of a page: Runtime.evaluate runs in the page's context, and the reply carries a JSON copy of the value.
function browser(page) {
  const context = vm.createContext({ document: page });
  const inputs = [];
  const send = async (method, params) => {
    if (method === 'Input.dispatchMouseEvent') {
      inputs.push(params);
      return {};
    }
    if (method !== 'Runtime.evaluate') {
      throw new Error(`the fake page has no ${method}`);
    }
    page.reads += 1;
    page.onRead?.(page.reads);
    try {
      const value = vm.runInContext(params.expression, context);
      return { result: { value: JSON.parse(JSON.stringify(value ?? null)) } };
    } catch (error) {
      return { exceptionDetails: { text: 'Uncaught', exception: { description: error.message } } };
    }
  };
  return { send, inputs };
}

const GEAR = '#row-2 button.gear';

function pressCenter(box) {
  return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
}

test('the helper presses at the center of a visible element and scrolls it into view first', async () => {
  const gear = new FakeElement('button', { matches: GEAR, classes: ['gear'] });
  const page = new FakePage([gear]);
  const { send, inputs } = browser(page);
  const result = await clickTarget(send, { selector: GEAR, waitMs: 2000 });
  const center = pressCenter(BOX);
  assert.deepEqual(
    inputs.map((event) => [event.type, event.x, event.y]),
    [
      ['mouseMoved', center.x, center.y],
      ['mousePressed', center.x, center.y],
      ['mouseReleased', center.x, center.y],
    ],
  );
  assert.deepEqual(inputs[1], { type: 'mousePressed', x: center.x, y: center.y, button: 'left', clickCount: 1 });
  assert.equal(gear.scrolls, 1);
  assert.equal(result.selector, GEAR);
  assert.equal(result.target, 'button.gear');
});

test('the helper waits within the bound for an element that appears, then clicks it', async () => {
  const gear = new FakeElement('button', { matches: GEAR, classes: ['gear'] });
  const page = new FakePage([]);
  page.onRead = (count) => {
    if (count === 3) page.roots.push(gear);
  };
  const { send, inputs } = browser(page);
  await clickTarget(send, { selector: GEAR, waitMs: 5000 });
  assert.equal(page.reads, 4, 'two missing reads, the appearing read, and the read after the aim');
  assert.equal(inputs.length, 3);
});

test('the helper waits within the bound for a zero-size box to get a size, then clicks it', async () => {
  const gear = new FakeElement('button', { matches: GEAR, rect: { ...BOX, height: 0 } });
  const page = new FakePage([gear]);
  page.onRead = (count) => {
    if (count === 2) gear.rect = { ...BOX };
  };
  const { send, inputs } = browser(page);
  await clickTarget(send, { selector: GEAR, waitMs: 5000 });
  assert.equal(inputs.length, 3);
});

test('the helper refuses a missing element after the bound, naming the selector and the reason', async () => {
  const page = new FakePage([]);
  const { send, inputs } = browser(page);
  const started = Date.now();
  await assert.rejects(clickTarget(send, { selector: GEAR, waitMs: 300 }), (error) => {
    assert.ok(error instanceof ClickRefused);
    assert.match(error.message, /#row-2 button\.gear/);
    assert.match(error.message, /no element matches the selector within 300 ms/);
    assert.equal(error.covering, null);
    return true;
  });
  assert.ok(Date.now() - started >= 300, 'the helper waits the whole bound');
  assert.ok(page.reads > 1, 'the helper polls during the bound');
  assert.deepEqual(inputs, [], 'no press reaches the page');
});

test('the helper refuses a zero-size element after the bound, naming the selector and the reason', async () => {
  const gear = new FakeElement('button', { matches: GEAR, rect: { ...BOX, width: 0, height: 0 } });
  const page = new FakePage([gear]);
  const { send, inputs } = browser(page);
  await assert.rejects(clickTarget(send, { selector: GEAR, waitMs: 200 }), (error) => {
    assert.ok(error instanceof ClickRefused);
    assert.match(error.message, /#row-2 button\.gear/);
    assert.match(error.message, /its box has zero width or height/);
    return true;
  });
  assert.deepEqual(inputs, []);
});

test('the helper refuses a covered center and names the covering element', async () => {
  const gear = new FakeElement('button', { matches: GEAR, classes: ['gear'] });
  const banner = new FakeElement('div', {
    id: 'cookie-banner',
    classes: ['modal', 'open'],
    rect: { left: 0, top: 0, width: 400, height: 400 },
  });
  const page = new FakePage([gear, banner]);
  const { send, inputs } = browser(page);
  await assert.rejects(clickTarget(send, { selector: GEAR, waitMs: 200 }), (error) => {
    assert.ok(error instanceof ClickRefused);
    assert.match(error.message, /#row-2 button\.gear/);
    assert.match(error.message, /covered by div#cookie-banner\.modal\.open/);
    assert.equal(error.covering, 'div#cookie-banner.modal.open');
    return true;
  });
  assert.deepEqual(inputs, []);
});

test('an overlay over only the edge of the box does not refuse, because the center is what the press hits', async () => {
  const gear = new FakeElement('button', { matches: GEAR, classes: ['gear'] });
  const edge = new FakeElement('div', { id: 'edge', rect: { left: 51, top: 0, width: 200, height: 400 } });
  const page = new FakePage([gear, edge]);
  const { send, inputs } = browser(page);
  await clickTarget(send, { selector: GEAR, waitMs: 200 });
  assert.equal(inputs.length, 3);
});

test('a descendant at the center counts as the target, so its click is not refused', async () => {
  const icon = new FakeElement('svg', { rect: { left: 40, top: 12, width: 20, height: 12 } });
  const gear = new FakeElement('button', { matches: GEAR, classes: ['gear'], children: [icon] });
  const page = new FakePage([gear]);
  const { send, inputs } = browser(page);
  await clickTarget(send, { selector: GEAR, waitMs: 200 });
  assert.deepEqual([inputs[1].x, inputs[1].y], [50, 22]);
});

test('a later click presses at the box the page shows then, not at the box of an earlier click', async () => {
  const page = new FakePage([new FakeElement('button', { matches: GEAR, rect: BOX })]);
  const { send, inputs } = browser(page);
  await clickTarget(send, { selector: GEAR, waitMs: 200 });
  // The row moves between two clicks, as a collapsed tree that expands does.
  page.roots = [new FakeElement('button', { matches: GEAR, rect: { left: 10, top: 300, width: 80, height: 24 } })];
  await clickTarget(send, { selector: GEAR, waitMs: 200 });
  assert.deepEqual([inputs[1].x, inputs[1].y], [50, 22]);
  assert.deepEqual([inputs[4].x, inputs[4].y], [50, 312]);
});

test('a selector the page cannot parse fails the call as an error, not as a refused click', async () => {
  const page = new FakePage([]);
  page.invalidSelectors.add('button[');
  const { send, inputs } = browser(page);
  await assert.rejects(clickTarget(send, { selector: 'button[', waitMs: 200 }), (error) => {
    assert.ok(!(error instanceof ClickRefused));
    assert.match(error.message, /the page threw while the click helper read its target/);
    return true;
  });
  assert.deepEqual(inputs, []);
});

test('the helper refuses to run without a bound, so a missing waitMs cannot wait forever', async () => {
  const page = new FakePage([new FakeElement('button', { matches: GEAR })]);
  const { send, inputs } = browser(page);
  await assert.rejects(clickTarget(send, { selector: GEAR }), TypeError);
  assert.equal(page.reads, 0);
  assert.deepEqual(inputs, []);
});
