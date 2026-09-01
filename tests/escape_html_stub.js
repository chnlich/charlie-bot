// ---------------------------------------------------------------------------
// Stand-ins for the browser `escapeHtml` (web/static/js/chat/shared.js) in the
// vm render tests. The real function escapes through
// `document.createElement('div')`, so it cannot run without a DOM, and its
// textContent -> innerHTML round-trip escapes only `& < >` — `escapeHtmlText`
// mirrors that one-for-one. Use it for sandbox `escapeHtml` globals:
// production interpolates the output into single-quoted JS inside onclick
// attributes (sidebar/groups.js openCronEditor/startRename), so a
// quote-escaping fake hides a break the browser would hit.
// `escapeHtml` keeps the `"` and `'` escapes for constructing expected values
// in attribute assertions (`href="..."` regexes) and, wrapped as
// `(value) => escapeHtml(value == null ? '' : String(value))`, for sandbox
// `escapeHtmlAttr` globals, where it reproduces production escapeHtmlAttr
// exactly.
// ---------------------------------------------------------------------------
function escapeHtmlText(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;');
}

function escapeHtml(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

module.exports = { escapeHtml, escapeHtmlText };
