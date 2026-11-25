// The click helper of an improve-ux side's driver. A driver clicks a control through clickTarget, which resolves the
// control's selector at the moment of the click and presses at the center of the box the page shows then. A click never
// aims at coordinates that an earlier load or list recorded.
//
// clickTarget waits up to the bound the caller states for a box of nonzero size. It refuses the click, with the selector
// and the reason, when no such box appears, when the center of the box is outside the viewport, or when another element
// covers that center. A refused click is a ClickRefused, a driver result that the caller records and does not count as a
// product defect. Any other failure, such as a selector the page cannot parse, is an Error and propagates.
//
// The helper reads the page through the CDP session's send(method, params), as installWriteGuard does in
// dev/improve/ux-frame-guard.mjs. The kit does not launch the driver, so loading this module is the driver's duty.
// `node --test dev/improve/ux-click-target.test.mjs` runs the helper against a fake page.

const POLL_MS = 100;

const BOX_REASON = {
  missing: 'no element matches the selector',
  'zero-size': 'its box has zero width or height',
};

// A click the helper refuses. The driver records it with its selector and reason; `covering` names the element that
// covers the center of the box, or is null when the refusal is about the box itself.
export class ClickRefused extends Error {
  constructor(selector, reason, covering) {
    super(`click refused on ${selector}: ${reason}`);
    this.name = 'ClickRefused';
    this.selector = selector;
    this.reason = reason;
    this.covering = covering;
  }
}

// Runs in the page alone, so it names no module binding. Reads the box without side effects: 'missing', 'zero-size' or
// 'shown'.
function probeBox(selector) {
  const el = document.querySelector(selector);
  if (el === null) return 'missing';
  const rect = el.getBoundingClientRect();
  return rect.width > 0 && rect.height > 0 ? 'shown' : 'zero-size';
}

// Runs in the page alone. Scrolls the target into view, then reports the point a press at its center would hit: `top`
// is null when that point is outside the viewport, and `own` says whether the hit is the target or one of its
// descendants. `name` describes an element as its tag, id and first classes.
function aimAt(selector) {
  const el = document.querySelector(selector);
  if (el === null) return { box: 'missing' };
  const before = el.getBoundingClientRect();
  if (before.width === 0 || before.height === 0) return { box: 'zero-size' };
  el.scrollIntoView({ block: 'center', inline: 'center' });
  const rect = el.getBoundingClientRect();
  const x = rect.left + rect.width / 2;
  const y = rect.top + rect.height / 2;
  const name = (node) => `${node.tagName.toLowerCase()}${node.id ? `#${node.id}` : ''}`
    + [...node.classList].slice(0, 3).map((cls) => `.${cls}`).join('');
  const top = document.elementFromPoint(x, y);
  return {
    box: 'shown',
    x,
    y,
    target: name(el),
    top: top === null ? null : { own: top === el || el.contains(top), name: name(top) },
  };
}

// The page's result of a read. A read that throws is a driver fault, so it fails the call.
async function evaluate(send, expression) {
  const reply = await send('Runtime.evaluate', { expression, returnByValue: true });
  if (reply.exceptionDetails) {
    const thrown = reply.exceptionDetails.exception?.description ?? reply.exceptionDetails.text;
    throw new Error(`the page threw while the click helper read its target: ${thrown}`);
  }
  return reply.result.value;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Clicks the element that `selector` names, with the left button, as the page shows it at the moment of the click.
// `send` is the CDP session's send(method, params). `waitMs` is the bound, in milliseconds, for the element to show a
// box of nonzero size; the driver states it on each call. Resolves to { selector, x, y, target } once the press is sent.
export async function clickTarget(send, { selector, waitMs }) {
  if (!Number.isFinite(waitMs) || waitMs < 0) {
    throw new TypeError(`clickTarget needs waitMs, the bound in milliseconds for a box to show; got ${waitMs}`);
  }
  const probe = `(${probeBox})(${JSON.stringify(selector)})`;
  const deadline = Date.now() + waitMs;
  let box = await evaluate(send, probe);
  while (box !== 'shown' && Date.now() < deadline) {
    await sleep(POLL_MS);
    box = await evaluate(send, probe);
  }
  if (box !== 'shown') {
    throw new ClickRefused(selector, `${BOX_REASON[box]} within ${waitMs} ms`, null);
  }

  const aim = await evaluate(send, `(${aimAt})(${JSON.stringify(selector)})`);
  if (aim.box !== 'shown') {
    throw new ClickRefused(selector, BOX_REASON[aim.box], null);
  }
  if (aim.top === null) {
    throw new ClickRefused(selector, 'the center of its box is outside the viewport', null);
  }
  if (!aim.top.own) {
    throw new ClickRefused(
      selector,
      `the element at the center of its box is covered by ${aim.top.name}`,
      aim.top.name,
    );
  }

  const press = { x: aim.x, y: aim.y, button: 'left', clickCount: 1 };
  await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: aim.x, y: aim.y });
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', ...press });
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...press });
  return { selector, x: aim.x, y: aim.y, target: aim.target };
}
