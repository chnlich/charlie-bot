const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");

const template = readFileSync(path.join(__dirname, "../features/host_auth/templates/host_auth.html"), "utf8");

test("host-auth keeps every host value in three cells, the renewal line inside Status", () => {
  const headers = [...template.matchAll(/<th\b[^>]*>([^<]*)<\/th>/g)].map((match) => match[1]);
  assert.equal(headers.length, 3);

  const row = template.match(/{% for h in hosts %}\s*<tr>([\s\S]*?)<\/tr>/)?.[1];
  assert.ok(row, "host row template exists");
  const [hostCell, statusCell, detailCell] = row.split(/<td\b/).slice(1);
  assert.deepEqual([...row.matchAll(/data-l="([^"]*)"/g)].map((match) => match[1]), headers);

  for (const value of ["h.alias", "h.hostname"]) {
    assert.ok(hostCell.includes(value), `${value} stays in the host cell`);
  }
  for (const value of [
    "h.status_label",
    "h.remaining",
    "h.expires",
    '"counting"',
    '"expired"',
    '"no_baseline"',
    '"pending_okta"',
  ]) {
    assert.ok(statusCell.includes(value), `${value} stays in the status cell`);
  }
  for (const value of ["h.detail_lines", "h.renew_login", "h.renew_cmd"]) {
    assert.ok(detailCell.includes(value), `${value} stays in the detail cell`);
  }
});

test("host-auth states its four facts in one caption under the subtitle", () => {
  assert.doesNotMatch(template, /class="mtag"/);
  const facts = template.match(/<p class="facts">([\s\S]*?)<\/p>/)?.[1];
  assert.ok(facts, "the facts caption exists");
  for (const value of ["last_probe", "interval_label", "backoff_label", "ttl_label"]) {
    assert.ok(facts.includes(value), `${value} stays in the caption`);
  }
});
