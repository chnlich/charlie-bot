// The input profile of an improve-ux side. A window size stands for one device, so each capture and click at that size
// runs under the device's input mode: a desktop size reports a fine pointer that hovers, and the phone size reports
// touch with no hover.
//
// The desktop mode is the browser's own default, and the browser's launch arguments set it: the driver starts its
// browser with INPUT_BROWSER_ARGS. The Playwright chromium-1248 build that the round pins ignores the hover and pointer
// features of Emulation.setEmulatedMedia, so no per-size call can set the desktop mode. Touch emulation does work per
// size, and it overrides the launch default for the phone size.
//
// applyInputProfile(send, size) sets the size's viewport and its touch emulation. It sets both on each call, so a size
// keeps nothing from the size before it. checkInputProfile(send, size) reads the loaded page's matchMedia answers for
// the hover and pointer queries. It refuses, with an InputProfileRefused that names the size and both readings, when
// the answers differ from the profile; a browser started without INPUT_BROWSER_ARGS refuses the desktop sizes. A driver
// checks each size before its first capture there, and it captures only after the check returns.
//
// `send` is the CDP session's send(method, params), as dev/improve/ux-frame-guard.mjs takes it. The kit does not launch
// the driver, so loading this module is the driver's duty. `node --test dev/improve/ux-input-profile.test.mjs` runs the
// profile against a fake page.

// The window sizes that `charliebot land --help` lists for the Web UI Screenshots rule, each with its device's input
// mode. A touch size is a phone.
export const WINDOW_SIZES = [
  { width: 1536, height: 900, touch: false },
  { width: 1400, height: 1000, touch: false },
  { width: 390, height: 844, touch: true },
];

// The launch arguments of a driver's browser. The blink setting names a primary pointer that is a mouse and a primary
// hover type that hovers, so a desktop size's page reports `(pointer: fine)` and `(hover: hover)`.
export const INPUT_BROWSER_ARGS = ['--blink-settings=primaryHoverType=2,primaryPointerType=4'];

const MAX_TOUCH_POINTS = 5;

// The media queries that the check reads, in the order of the readings it reports.
const QUERIES = ['(hover: hover)', '(hover: none)', '(pointer: fine)', '(pointer: coarse)'];

// A page that does not take the profile. Names the size and both readings, so the driver's record shows which size
// failed and what the page answered.
export class InputProfileRefused extends Error {
  constructor(size, expected, actual) {
    super(
      `the input profile of ${size} did not take effect: expected ${show(expected)}; `
      + `the page reads ${show(actual)}`,
    );
    this.name = 'InputProfileRefused';
    this.size = size;
    this.expected = expected;
    this.actual = actual;
  }
}

function sizeName({ width, height }) {
  return `${width}x${height}`;
}

function show(readings) {
  return QUERIES.map((query) => `${query} ${readings[query]}`).join(', ');
}

// The profile of SIZE, a { width, height } pair. A size that the profile does not name fails before any call reaches
// the page.
function profileOf(size) {
  const profile = WINDOW_SIZES.find((entry) => entry.width === size.width && entry.height === size.height);
  if (profile === undefined) {
    const named = WINDOW_SIZES.map(sizeName).join(', ');
    throw new Error(`the input profile names no window size ${sizeName(size)}; it names ${named}`);
  }
  return profile;
}

// The readings that a profile gives, keyed by QUERIES. A touch device has coarse pointers and no hover.
function expectedReadings(touch) {
  return {
    '(hover: hover)': !touch,
    '(hover: none)': touch,
    '(pointer: fine)': !touch,
    '(pointer: coarse)': touch,
  };
}

// Runs in the page alone, so it names no module binding. Returns each query's answer.
function readQueries(queries) {
  return Object.fromEntries(queries.map((query) => [query, matchMedia(query).matches]));
}

// Sets the viewport and touch emulation of SIZE's device on the page of `send`.
export async function applyInputProfile(send, size) {
  const profile = profileOf(size);
  await send('Emulation.setDeviceMetricsOverride', {
    width: profile.width,
    height: profile.height,
    deviceScaleFactor: 1,
    mobile: profile.touch,
  });
  await send('Emulation.setTouchEmulationEnabled', { enabled: profile.touch, maxTouchPoints: MAX_TOUCH_POINTS });
}

// Reads the loaded page's matchMedia answers and checks them against SIZE's profile. Resolves to the readings, which
// the driver records as evidence, when they match. Rejects with an InputProfileRefused when they do not.
export async function checkInputProfile(send, size) {
  const profile = profileOf(size);
  const reply = await send('Runtime.evaluate', {
    expression: `(${readQueries})(${JSON.stringify(QUERIES)})`,
    returnByValue: true,
  });
  if (reply.exceptionDetails) {
    const thrown = reply.exceptionDetails.exception?.description ?? reply.exceptionDetails.text;
    throw new Error(`the page threw while the input profile read its media queries: ${thrown}`);
  }
  const readings = reply.result.value;
  const expected = expectedReadings(profile.touch);
  if (QUERIES.some((query) => readings[query] !== expected[query])) {
    throw new InputProfileRefused(sizeName(size), expected, readings);
  }
  return readings;
}
