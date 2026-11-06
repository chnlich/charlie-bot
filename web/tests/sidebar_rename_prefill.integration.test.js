// Rename-prefill mechanism tests: the rename input must prefill the session's
// current name, read live from the DOM at open time. No rename handler string
// may carry session state, the class of bug where a second rename prefills the
// stale render-time name. The groups.js vm harness is shared:
// web/tests/sidebar_groups_context_stub.js.
const assert = require('node:assert/strict');
const test = require('node:test');

test('header handler in the template reads the live global, no server-side interpolation', () => {
  const fs = require('node:fs');
  const path = require('node:path');
  const html = fs.readFileSync(path.join(__dirname, '..', 'templates', 'index.html'), 'utf8');
  const attr = html.match(/onclick="(startRename\([^"]*)"/);
  assert.ok(attr, 'index.html header is missing a startRename onclick handler');
  assert.equal(attr[1], 'startRename(event, SESSION_ID)',
    'header handler must be the static global-reading form with no interpolated session state');
});
