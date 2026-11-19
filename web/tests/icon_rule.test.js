// ---------------------------------------------------------------------------
// The icon rule's table checks (plan 4.2, the long-term rule). Each icon a CharlieBot page shows is drawn by the
// icon module from web/static/icons/icons.json and the Lucide files it names. The scan itself — the repository-wide
// test that keeps icons off the pages' own source — reads every scanned source and runs as an integration suite,
// web/tests/icon_rule.integration.test.js; this file holds the three checks that fit the unit-time gate. The
// scanner and the failure text live in icon_rule_scan.js.
// ---------------------------------------------------------------------------
'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { iconCallSites, failure } = require('./icon_rule_scan.js');

const ROOT = path.join(__dirname, '..', '..');
const ICON_DIR = path.join(ROOT, 'web', 'static', 'icons');
const LUCIDE_DIR = path.join(ICON_DIR, 'lucide');

function iconTable() {
  return JSON.parse(fs.readFileSync(path.join(ICON_DIR, 'icons.json'), 'utf8'));
}

const ISC_LINE = /^<!-- @license lucide-static \S+ - ISC -->$/;

// A call-site key the table lacks may stand only where the icon module's own test calls it on purpose.
const CALL_EXCEPTIONS = [
  {
    file: 'rust/crates/runtime/registry/src/icons.rs',
    text: '"not-in-the-table"',
    reason: 'the icon module\'s own test of an unknown key: the error is what it checks',
  },
];

function excusedCall(site) {
  return CALL_EXCEPTIONS.some((exception) => exception.file === site.file && site.source.includes(exception.text));
}

test('every icon call names a meaning the table defines', () => {
  const table = iconTable();
  const report = iconCallSites(ROOT)
    .filter((site) => !Object.hasOwn(table, site.key) && !excusedCall(site))
    .map((site) => `  ${site.file}:${site.line}  ${site.api}('${site.key}')`);
  assert.equal(report.length, 0, failure(`${report.length} calls name a meaning the table does not define`, report));
});

test('no two meanings draw the same Lucide file in the same fill state', () => {
  const seen = new Map();
  const report = [];
  for (const [meaning, { lucide, fill = false }] of Object.entries(iconTable())) {
    const pair = `${lucide} fill=${fill === true}`;
    if (seen.has(pair)) report.push(`  ${seen.get(pair)} and ${meaning} both draw ${pair}`);
    else seen.set(pair, meaning);
  }
  assert.equal(report.length, 0, failure(`${report.length} meanings share a (lucide, fill) pair`, report));
});

test('every Lucide file is named by the table, carries its ISC license line, and the package LICENSE exists', () => {
  const files = fs.readdirSync(LUCIDE_DIR).filter((name) => name.endsWith('.svg')).map((name) => name.slice(0, -4));
  const used = new Set(Object.values(iconTable()).map((icon) => icon.lucide));
  const report = [
    ...files.filter((name) => !used.has(name)).map((name) => `  lucide/${name}.svg is not named by icons.json`),
    ...[...used]
      .filter((name) => !files.includes(name))
      .map((name) => `  icons.json names lucide/${name}.svg, missing`),
    ...files
      .filter((name) => !ISC_LINE.test(fs.readFileSync(path.join(LUCIDE_DIR, `${name}.svg`), 'utf8').split('\n', 1)[0]))
      .map((name) => `  lucide/${name}.svg has no ISC license line`),
  ];
  if (!fs.existsSync(path.join(ICON_DIR, 'LICENSE'))) report.push('  web/static/icons/LICENSE is missing');
  assert.equal(report.length, 0, failure(`${report.length} Lucide files do not match the table`, report));
});
