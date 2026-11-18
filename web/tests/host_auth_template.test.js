const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");

const template = readFileSync(path.join(__dirname, "../features/host_auth/templates/host_auth.html"), "utf8");
const rowsSource = readFileSync(path.join(__dirname, "../../rust/crates/features/host_auth/src/rows.rs"), "utf8");
const pageSource = readFileSync(path.join(__dirname, "../../rust/crates/features/host_auth/src/page.rs"), "utf8");

// The keys of the object `row_json` builds: each sits at the start of its line, as `"key": value,`.
function rowFields() {
  const body = rowsSource.slice(rowsSource.indexOf("fn row_json"), rowsSource.indexOf("fn code_or_none"));
  return new Set([...body.matchAll(/^\s*"([a-z_]+)":/gm)].map((match) => match[1]));
}

// The keys of the template context `page.rs` builds.
function contextKeys() {
  const body = pageSource.slice(pageSource.indexOf("fn context"), pageSource.indexOf("#[cfg(test)]"));
  return new Set([...body.matchAll(/^\s*"([a-z_]+)":/gm)].map((match) => match[1]));
}

test("host-auth draws one row loop whose cells name the three columns", () => {
  const headers = [...template.matchAll(/<th\b[^>]*>([^<]*)<\/th>/g)].map((match) => match[1]);
  assert.equal(headers.length, 3);

  const row = template.match(/{% for row in rows %}\s*<tr\b[^>]*>([\s\S]*?)<\/tr>/)?.[1];
  assert.ok(row, "the row loop exists");
  assert.deepEqual([...row.matchAll(/data-l="([^"]*)"/g)].map((match) => match[1]), headers);
});

test("every field the template reads from a row is a field the row JSON carries", () => {
  const read = new Set([
    ...[...template.matchAll(/\brow\.([a-z_]+)/g)].map((match) => match[1]),
    ...[...template.matchAll(/data-(?:f|show|amber|href)="([a-z_]+)"/g)].map((match) => match[1]),
  ]);
  assert.ok(read.size > 0, "the template reads row fields");
  const fields = rowFields();
  for (const field of read) {
    assert.ok(fields.has(field), `${field} is not a field of the row JSON`);
  }
});

test("every top-level name the template reads is a key of the page context", () => {
  const loopNames = new Set(["row", "loop"]);
  const read = new Set([
    ...[...template.matchAll(/{{-?\s*([a-z_]+)/g)].map((match) => match[1]),
    ...[...template.matchAll(/{%-?\s*(?:if|elif|for)\s+(?:not\s+)?([a-z_]+)/g)].map((match) => match[1]),
    ...[...template.matchAll(/{%-?\s*for\s+[a-z_]+\s+in\s+([a-z_]+)/g)].map((match) => match[1]),
  ]);
  const keys = contextKeys();
  for (const name of read) {
    if (!loopNames.has(name)) {
      assert.ok(keys.has(name), `${name} is not a key of the page context`);
    }
  }
});

test("the page refreshes login rows one at a time and follows active rows", () => {
  assert.match(template, /data-kind="\{\{ row\.kind \}\}"/);
  assert.match(template, /async function refreshRows\(rows\)[\s\S]*?await checkRow\(tr\)/);
  assert.match(template, /tr\.dataset\.kind === "login"/);
  assert.match(template, /if \(tr\.dataset\.busy\)\s*\{\s*follow\(tr, 0\)/);
  assert.match(template, /setTimeout\(tick, POLL_MS\)/);
});

test("row updates keep the status order and show counts with names in the header", () => {
  assert.match(template, /function reorderRows\(\)/);
  assert.match(template, /stateRank\[left\.dataset\.state\] - stateRank\[right\.dataset\.state\]/);
  assert.match(template, /\$\{label\} \(\$\{names\.length\}\):/);
  assert.match(template, /list\("Needs you", needs\).*list\("Broken", broken\)/);
});
