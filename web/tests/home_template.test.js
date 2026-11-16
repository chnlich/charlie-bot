const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");

const template = readFileSync(path.join(__dirname, "../templates/home.html"), "utf8");

const sectionGrid = (n) =>
  template.match(new RegExp(`<h2><span class="n">${n}</span>[\\s\\S]*?<div class="([^"]*)">`))?.[1];

test("home section 1 lays its cards in three columns from 900 px", () => {
  const grid = sectionGrid("1");
  assert.ok(grid, "section 1's grid exists");
  assert.ok(grid.split(" ").includes("page-cards"), "section 1's grid carries page-cards");

  assert.ok(
    /@media \(min-width:900px\) \{[^}]*\.page-cards \{ grid-template-columns:repeat\(3,minmax\(0,1fr\)\)/.test(template),
    "the 900 px rule gives section 1 three columns",
  );
});

test("section 2 keeps the shared auto-fill grid and its own route line", () => {
  const grid = sectionGrid("2");
  assert.ok(grid, "section 2's grid exists");
  assert.ok(!grid.split(" ").includes("page-cards"), "section 2's grid does not carry page-cards");

  const card = template.match(/{% for s in services %}([\s\S]*?){% endfor %}/)?.[1];
  assert.ok(card, "the service card template exists");
  assert.ok(card.includes('<span class="tgt">{{ s.url }}</span>'), "the service route keeps its own line");
});

test("each section-1 card shows its route on its name line and keeps its description", () => {
  const card = template.match(/{% for d in destinations %}([\s\S]*?){% endfor %}/)?.[1];
  assert.ok(card, "the section-1 card template exists");

  const head = card.match(/<span class="head">([\s\S]*?)<span class="desc">/)?.[1];
  assert.ok(head, "the name line runs to the description");
  for (const value of ["d.name", "d.url"]) {
    assert.ok(head.includes(value), `${value} stays on the name line`);
  }

  assert.ok(card.includes("d.description"), "the description stays on the card");
  const afterDesc = card.match(/<span class="desc">{{ d.description }}<\/span>\s*([\s\S]*)$/)?.[1];
  assert.ok(afterDesc !== undefined && !afterDesc.includes('class="tgt"'), "no route line after the description");
});
