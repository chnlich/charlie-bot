const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

// The events viewer's highlighter, run as a browser classic script: its functions become globals of the context.
function loadHighlighter() {
  const context = {};
  vm.createContext(context);
  const file = path.join(__dirname, '..', 'web', 'static', 'js', 'json-highlight.js');
  vm.runInContext(fs.readFileSync(file, 'utf8'), context, {filename: 'json-highlight.js'});
  return context;
}

const {syntaxHighlight} = loadHighlighter();

function countSpans(html, cls) {
  return html.split(`<span class="${cls}">`).length - 1;
}

test('an event renders keys and HTML-escaped string values, indented like JSON.stringify', () => {
  assert.equal(syntaxHighlight({type: 'tool_result', content: 'a<b'}), [
    '{',
    '  <span class="json-key">"type":</span> <span class="json-str">"tool_result"</span>,',
    '  <span class="json-key">"content":</span> <span class="json-str">"a&lt;b"</span>',
    '}',
  ].join('\n'));
});

test('a 4,300,000-character plain string is one json-str span', () => {
  const html = syntaxHighlight({s: 'a'.repeat(4300000)});
  assert.equal(countSpans(html, 'json-str'), 1);
  assert.ok(html.endsWith(`<span class="json-str">"${'a'.repeat(4300000)}"</span>\n}`));
});

test('a 5,000,000-newline string is one json-str span', () => {
  const html = syntaxHighlight({s: '\n'.repeat(5000000)});
  assert.equal(countSpans(html, 'json-str'), 1);
  assert.ok(html.endsWith(`<span class="json-str">"${'\\n'.repeat(5000000)}"</span>\n}`));
});

test('nested objects and arrays add two spaces per level', () => {
  assert.equal(syntaxHighlight({a: [{b: [1]}]}), [
    '{',
    '  <span class="json-key">"a":</span> [',
    '    {',
    '      <span class="json-key">"b":</span> [',
    '        <span class="json-num">1</span>',
    '      ]',
    '    }',
    '  ]',
    '}',
  ].join('\n'));
});

test('empty arrays and objects stay on one line', () => {
  assert.equal(syntaxHighlight({e: [], o: {}}), [
    '{',
    '  <span class="json-key">"e":</span> [],',
    '  <span class="json-key">"o":</span> {}',
    '}',
  ].join('\n'));
});

test('negative and exponent numbers use the JSON.stringify spelling', () => {
  assert.equal(syntaxHighlight({neg: -12.5, small: 1.5e-7, big: 1e21}), [
    '{',
    '  <span class="json-key">"neg":</span> <span class="json-num">-12.5</span>,',
    '  <span class="json-key">"small":</span> <span class="json-num">1.5e-7</span>,',
    '  <span class="json-key">"big":</span> <span class="json-num">1e+21</span>',
    '}',
  ].join('\n'));
});

test('booleans and null get their own classes', () => {
  assert.equal(syntaxHighlight({t: true, f: false, z: null}), [
    '{',
    '  <span class="json-key">"t":</span> <span class="json-bool">true</span>,',
    '  <span class="json-key">"f":</span> <span class="json-bool">false</span>,',
    '  <span class="json-key">"z":</span> <span class="json-null">null</span>',
    '}',
  ].join('\n'));
});

test('a key with quotes, a backslash, < and a newline is JSON-escaped, then HTML-escaped', () => {
  assert.equal(syntaxHighlight({'a"b\\c<\n': 1}), String.raw`{
  <span class="json-key">"a\"b\\c&lt;\n":</span> <span class="json-num">1</span>
}`);
});

test('string values escape &, < and >', () => {
  assert.equal(syntaxHighlight({s: 'a&b<c>d'}), [
    '{',
    '  <span class="json-key">"s":</span> <span class="json-str">"a&amp;b&lt;c&gt;d"</span>',
    '}',
  ].join('\n'));
});
