// ---------------------------------------------------------------------------
// Stand-in for the browser `escapeHtml` (web/static/js/chat/shared.js) in the
// vm render tests. The real function escapes through
// `document.createElement('div')`, so it cannot run without a DOM. This copy
// additionally escapes `"` and `'`: tests embed escaped text inside
// double-quoted attribute assertions (`href="..."` regexes).
// ---------------------------------------------------------------------------
function escapeHtml(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

module.exports = { escapeHtml };
