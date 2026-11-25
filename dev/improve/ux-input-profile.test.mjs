// Tests of the improve-ux input profile. The fake page follows the Chromium build the round pins: touch emulation sets
// a coarse pointer with no hover. Without it, a browser started with INPUT_BROWSER_ARGS reports a fine pointer that
// hovers, and a browser started without them reports neither. The fake keeps the viewport and the touch emulation that
// the Emulation calls set. A test file's name ends in .test.mjs, so the bench-lock naming check reads it as a test.

import assert from 'node:assert/strict';
import vm from 'node:vm';
import { test } from 'node:test';

import {
  applyInputProfile,
  checkInputProfile,
  INPUT_BROWSER_ARGS,
  InputProfileRefused,
  WINDOW_SIZES,
} from './ux-input-profile.mjs';

const DESKTOP = {
  '(hover: hover)': true,
  '(hover: none)': false,
  '(pointer: fine)': true,
  '(pointer: coarse)': false,
};
const PHONE = {
  '(hover: hover)': false,
  '(hover: none)': true,
  '(pointer: fine)': false,
  '(pointer: coarse)': true,
};
const NO_INPUT = {
  '(hover: hover)': false,
  '(hover: none)': true,
  '(pointer: fine)': false,
  '(pointer: coarse)': false,
};
const EXPECTED = new Map([
  ['1536x900', DESKTOP],
  ['1400x1000', DESKTOP],
  ['390x844', PHONE],
]);

const READ_ANSWERS = `({
  '(hover: hover)': matchMedia('(hover: hover)').matches,
  '(hover: none)': matchMedia('(hover: none)').matches,
  '(pointer: fine)': matchMedia('(pointer: fine)').matches,
  '(pointer: coarse)': matchMedia('(pointer: coarse)').matches,
})`;

const MEDIA_QUERY = /^\((hover|pointer): (\w+)\)$/;

// The answers of each input state. The state is the touch emulation first, then the launch arguments.
const ANSWERS = {
  touch: { hover: 'none', pointer: 'coarse' },
  mouse: { hover: 'hover', pointer: 'fine' },
  none: { hover: 'none', pointer: 'none' },
};

class FakePage {
  constructor({ launchArgs = [] } = {}) {
    this.launchedWithInputArgs = INPUT_BROWSER_ARGS.every((arg) => launchArgs.includes(arg));
    this.calls = [];
    this.viewport = null;
    this.touch = { enabled: false, maxTouchPoints: 1 };
    this.captures = 0;
  }

  matches(query) {
    const [, name, value] = MEDIA_QUERY.exec(query);
    let state = 'none';
    if (this.touch.enabled) state = 'touch';
    else if (this.launchedWithInputArgs) state = 'mouse';
    return ANSWERS[state][name] === value;
  }
}

// The CDP send of a fake page. Runtime.evaluate runs in the page's context, and its reply carries a JSON copy of
// the value.
function browser(page) {
  const context = vm.createContext({ matchMedia: (query) => ({ matches: page.matches(query) }) });
  return async (method, params) => {
    page.calls.push(method);
    switch (method) {
      case 'Emulation.setDeviceMetricsOverride':
        page.viewport = { width: params.width, height: params.height, mobile: params.mobile };
        return {};
      case 'Emulation.setTouchEmulationEnabled':
        page.touch = { enabled: params.enabled, maxTouchPoints: params.maxTouchPoints };
        return {};
      case 'Runtime.evaluate': {
        const value = vm.runInContext(params.expression, context);
        return { result: { value: JSON.parse(JSON.stringify(value)) } };
      }
      case 'Page.captureScreenshot':
        page.captures += 1;
        return { data: '' };
      default:
        throw new Error(`the fake page has no ${method}`);
    }
  };
}

const sizeKey = ({ width, height }) => `${width}x${height}`;
const DESKTOP_SIZE = WINDOW_SIZES.find((size) => !size.touch);
const PHONE_SIZE = WINDOW_SIZES.find((size) => size.touch);

// The driver's order for one capture: the profile, then the check on the loaded page, then the capture.
async function captureAt(send, size) {
  await applyInputProfile(send, size);
  await checkInputProfile(send, size);
  return send('Page.captureScreenshot', { format: 'png' });
}

test('each size profile gives the expected hover and pointer answers to matchMedia', async () => {
  for (const size of WINDOW_SIZES) {
    const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
    const send = browser(page);
    await applyInputProfile(send, size);
    const answers = await send('Runtime.evaluate', { expression: READ_ANSWERS, returnByValue: true });
    assert.deepEqual(answers.result.value, EXPECTED.get(sizeKey(size)), sizeKey(size));
  }
});

test('a size sets a viewport of its size, and touch emulation on only for the phone', async () => {
  for (const size of WINDOW_SIZES) {
    const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
    await applyInputProfile(browser(page), size);
    assert.deepEqual(page.viewport, { width: size.width, height: size.height, mobile: size.touch }, sizeKey(size));
    assert.equal(page.touch.enabled, size.touch, sizeKey(size));
  }
});

test('a profile applied after the phone leaves no touch emulation for a desktop size', async () => {
  const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
  const send = browser(page);
  await applyInputProfile(send, PHONE_SIZE);
  await applyInputProfile(send, DESKTOP_SIZE);
  const answers = await send('Runtime.evaluate', { expression: READ_ANSWERS, returnByValue: true });
  assert.deepEqual(answers.result.value, DESKTOP);
  assert.equal(page.touch.enabled, false);
  assert.equal(page.viewport.width, DESKTOP_SIZE.width);
});

test('the check resolves to the readings when the page answers the profile', async () => {
  for (const size of WINDOW_SIZES) {
    const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
    const send = browser(page);
    await applyInputProfile(send, size);
    assert.deepEqual(await checkInputProfile(send, size), EXPECTED.get(sizeKey(size)), sizeKey(size));
  }
});

test('a desktop size whose browser lacks the input arguments is refused, naming size and readings', async () => {
  const page = new FakePage();
  const send = browser(page);
  await applyInputProfile(send, DESKTOP_SIZE);
  await assert.rejects(checkInputProfile(send, DESKTOP_SIZE), (error) => {
    assert.ok(error instanceof InputProfileRefused);
    assert.match(error.message, /1536x900/);
    assert.deepEqual(error.expected, DESKTOP);
    assert.deepEqual(error.actual, NO_INPUT);
    assert.match(error.message, /\(pointer: fine\) false/, 'the page reading is named');
    return true;
  });
});

test('the phone size takes its profile in a browser started without the input arguments', async () => {
  const page = new FakePage();
  const send = browser(page);
  await applyInputProfile(send, PHONE_SIZE);
  assert.deepEqual(await checkInputProfile(send, PHONE_SIZE), PHONE);
});

test('a driver captures nothing at a desktop size whose browser lacks the input arguments', async () => {
  const page = new FakePage();
  await assert.rejects(captureAt(browser(page), DESKTOP_SIZE), InputProfileRefused);
  assert.equal(page.captures, 0, 'no capture reaches the page');
});

test('a driver captures once at each size in a browser started with the input arguments', async () => {
  for (const size of WINDOW_SIZES) {
    const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
    await captureAt(browser(page), size);
    assert.equal(page.captures, 1, sizeKey(size));
  }
});

test('a size the profile does not name is refused before any Emulation call', async () => {
  const page = new FakePage({ launchArgs: INPUT_BROWSER_ARGS });
  const send = browser(page);
  await assert.rejects(applyInputProfile(send, { width: 800, height: 600 }), /800x600/);
  await assert.rejects(checkInputProfile(send, { width: 800, height: 600 }), /800x600/);
  assert.deepEqual(page.calls, []);
});
