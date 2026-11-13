// ---------------------------------------------------------------------------
// The briefing-page fork fixture shared by the artifact-comments suites: the
// 11-12 waiting page's shape (h2 › div.fork › p.q > span.fn + p.rec) reduced to
// two forks, carrying the page's real strings. Built from the DOM stub's
// elements, assembled with appendChild so the class lookups (closest,
// querySelector), the commentable-block walk and the document-order walk all
// see the tree.
// ---------------------------------------------------------------------------

const {makeElement} = require('./artifact_comments_dom_stub');

const HEADING_11_12 = '想问（显示 10 项，另 17 项见下方计数）';
const REC_TEXT = '不带推荐：想问项按规则不给推荐，「做」或「不做」由你定。';
const Q1_TEXT = '要不要处理 Yuesheng 的请求：在 11/12（周四，今天）前更新你负责的 4 个过期 Linear 项目？';
const Q2_TEXT = '要不要回应 FPT 的 CPU 容量答复：明年前没有容量，之后也只有 1000 核，Charlie 问作业能不能跑更久、并发更低？';

function textNode(text) {
  return {nodeType: 3, textContent: text};
}

function blockWithText(text) {
  const el = makeElement();
  el.childNodes = [textNode(text)];
  return el;
}

// One fork of the briefing grammar: the numbered question line (p.q with its
// span.fn) and the recommendation line (p.rec), each a commentable block. The
// question text rides a text node of its own, the way the number span and the
// question text sit side by side in the page source.
function makeFork(number, questionText) {
  const fn = blockWithText(number);
  fn.tagName = 'SPAN';
  fn.className = 'fn';
  fn.display = 'inline';
  const q = makeElement();
  q.tagName = 'P';
  q.className = 'q';
  q.appendChild(fn);
  q.appendChild(textNode(' ' + questionText));
  const rec = blockWithText(REC_TEXT);
  rec.tagName = 'P';
  rec.className = 'rec';
  const forkEl = makeElement();
  forkEl.className = 'fork';
  forkEl.appendChild(q);
  forkEl.appendChild(rec);
  return {fork: forkEl, q, rec};
}

// The 11-12 briefing page's 想问 section reduced to two forks under one h2: the
// same rec line in both, so a locator that names no fork cannot tell them apart.
function makeForkPage() {
  const heading = blockWithText(HEADING_11_12);
  heading.tagName = 'H2';
  const fork1 = makeFork('1', Q1_TEXT);
  const fork2 = makeFork('2', Q2_TEXT);
  return {heading, fork1, fork2, children: [heading, fork1.fork, fork2.fork]};
}

module.exports = {HEADING_11_12, REC_TEXT, Q1_TEXT, Q2_TEXT, makeFork, makeForkPage};
