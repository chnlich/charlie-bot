const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const testDirectory = __dirname;
const webDirectory = path.resolve(testDirectory, '..');
const iconDirectory = path.join(webDirectory, 'static', 'icons');
const iconTable = JSON.parse(fs.readFileSync(path.join(iconDirectory, 'icons.json'), 'utf8'));
const fixture = JSON.parse(fs.readFileSync(path.join(testDirectory, 'icons_parity.json'), 'utf8'));
const iconScript = fs.readFileSync(path.join(webDirectory, 'static', 'js', 'icons.js'), 'utf8');

test('the icon table has the approved meanings and licensed Lucide files', () => {
  const expectedKeys = [
    'add', 'archive', 'attachment', 'back', 'clone', 'close', 'comment', 'delegated', 'delete', 'diff', 'disclosure',
    'disclosure-open', 'done', 'download', 'dot', 'events', 'expand-down', 'expand-up', 'explain', 'failed', 'file',
    'folder', 'home', 'leads-to', 'loading', 'menu', 'more', 'notification', 'open', 'plan', 'rate-down', 'rate-up',
    'resize', 'retry', 'ring', 'scheduled', 'scroll-bottom', 'send', 'settings', 'star', 'starred', 'stop',
    'subtasks-running', 'unarchive', 'undo', 'voice', 'waiting', 'waiting-on-you', 'warning', 'worker',
  ];
  assert.deepEqual(Object.keys(iconTable).sort(), expectedKeys.sort());
  assert.deepEqual(
    Object.entries(iconTable).filter(([, entry]) => entry.fill === true).map(([key]) => key).sort(),
    ['dot', 'starred'],
  );

  const lucideDirectory = path.join(iconDirectory, 'lucide');
  const lucideNames = [...new Set(Object.values(iconTable).map((entry) => entry.lucide))].sort();
  assert.equal(lucideNames.length, 48);
  assert.deepEqual(fs.readdirSync(lucideDirectory).filter((name) => name.endsWith('.svg')).sort(),
    lucideNames.map((name) => `${name}.svg`).sort());
  assert.ok(fs.statSync(path.join(iconDirectory, 'LICENSE')).isFile());
  for (const name of lucideNames) {
    const source = fs.readFileSync(path.join(lucideDirectory, `${name}.svg`), 'utf8');
    assert.match(source.split(/\r?\n/, 1)[0], /^<!-- @license lucide-static v1\.76\.0 - ISC -->$/);
  }
});

test('cbIcon matches Icons::svg outputs and fails loudly on an unknown meaning', () => {
  const window = {};
  const context = vm.createContext({ window });
  vm.runInContext(fixture.script, context);
  vm.runInContext(iconScript, context);

  for (const item of fixture.cases) {
    assert.equal(window.cbIcon(item.key, item.size, item.cls, item.attrs), item.svg);
  }
  assert.throws(() => window.cbIcon('missing-meaning', 16), /missing-meaning/);
  assert.throws(() => window.cbIcon('close', 0), /close/);
});
