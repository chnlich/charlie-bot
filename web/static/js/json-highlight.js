// JSON syntax highlighting for the events viewer. The value is walked, not matched: a regex over the JSON text
// keeps one V8 backtrack record per character of a string, and in Chrome 153 a string over about 4.19M characters
// throws RangeError. Loaded as a classic script before the viewer's inline script; both functions are globals.

// Returns the highlighted HTML of obj, indented as JSON.stringify(obj, null, 2) indents it.
function syntaxHighlight(obj) {
  return renderValue(obj, '');
}

function escapeHtml(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function highlightSpan(cls, text) {
  return `<span class="${cls}">${escapeHtml(text)}</span>`;
}

// Children are visited with for loops, not map callbacks: each nesting level costs one stack frame. A map
// callback would add a second frame per level and lower the nesting depth the viewer can draw.
function renderValue(value, indent) {
  if (value === null) return highlightSpan('json-null', 'null');
  if (typeof value === 'string') return highlightSpan('json-str', JSON.stringify(value));
  if (typeof value === 'number') return highlightSpan('json-num', JSON.stringify(value));
  if (typeof value === 'boolean') return highlightSpan('json-bool', String(value));
  const pad = indent + '  ';
  let out = '';
  if (Array.isArray(value)) {
    if (!value.length) return '[]';
    for (let i = 0; i < value.length; i++) out += (i ? ',\n' : '') + pad + renderValue(value[i], pad);
    return '[\n' + out + '\n' + indent + ']';
  }
  const keys = Object.keys(value);
  if (!keys.length) return '{}';
  for (let i = 0; i < keys.length; i++) {
    const key = highlightSpan('json-key', JSON.stringify(keys[i]) + ':');
    out += (i ? ',\n' : '') + pad + key + ' ' + renderValue(value[keys[i]], pad);
  }
  return '{\n' + out + '\n' + indent + '}';
}
