// ---------------------------------------------------------------------------
// The icon rule's repository scan (plan 4.2, the long-term rule). Each icon a CharlieBot page shows is drawn by
// the icon module from web/static/icons/icons.json and the Lucide files it names. The scan is in
// icon_rule_scan.js; this file holds the scan test and the exception list.
//
// This is an integration suite, not a unit suite: the scan reads every scanned source with several passes, and
// that does not fit the 0.1 s unit-time gate (dev/check-unit-test-time.mjs). The Integration workflow runs every
// *.integration.test.* on implement landing PRs and on push to main, and selects this file for web/, prompts and
// rust source changes (dev/integration-paths.toml).
// ---------------------------------------------------------------------------
'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');

const { scanIconUses, failure, GUIDE } = require('./icon_rule_scan.js');

// The scanned tree: the repository root, whether the test runs from web/ or the repository root.
const ROOT = path.join(__dirname, '..', '..');

// The reasons an excused line is not an icon.
const MODULE_REASON = 'the icon module: the code that draws its icons, and its tests';
const PLAN_COPY_REASON = "the plan template's copy of the icon module's flow arrow; icons.rs asserts the copies match";
const DIAGRAM_REASON = 'a data diagram in a prompt template: a chart of frames, not an icon';
const CHART_REASON = "the concurrency chart's SVG: a data chart, not an icon";
const FIXTURE_REASON = 'an artifact check fixture: the SVG or flow text the check reads';
const CHAT_TEXT_REASON = '▸ or ↳ text that a chat message carries: text sent to the chat, not drawn as an icon';
const TIMES_REASON = 'a count written as ×N in text';
const BREADCRUMB_REASON = 'the › between a comment heading and its question: text, not an icon';
const REACTION_REASON = "a chat platform's reaction emoji or its test fixture: the platform's own reaction";
const TEST_DATA_REASON = 'test data for text escaping: the character under test, not an icon';
const PROSE_REASON = 'an ellipsis or arrow inside a sentence or a label: text, not an icon';
const TELEGRAM_ALERT_REASON = 'a cron load-failure alert sent to Telegram: notification text, not drawn as an icon';

function byFile(file, rows) {
  return rows.map(([text, reason]) => ({ file, text, reason }));
}

// Each exception excuses one line: the file, the exact text the line contains, and the reason. A finding is excused
// only when its file and its line both match an entry, so a new icon-like line in a listed file still fails.
const EXCEPTIONS = [
  ...byFile('prompts/debug_template.html', [
    ["<svg viewBox=\"0 0 930 110\" xmlns=\"http://www.w3.org/2000/svg\" role", DIAGRAM_REASON],
    ["refY=\"4.5\" orient=\"auto\"><path d=\"M0,0 L9,4.5 L0,9 z\" fill=\"#5b677", DIAGRAM_REASON],
    ["<rect x=\"10\" y=\"22\" width=\"190\" height=\"64\" rx=\"8\" fill=\"#ffffff\" ", DIAGRAM_REASON],
    ["<line x1=\"202\" y1=\"54\" x2=\"250\" y2=\"54\" stroke=\"#5b6774\" stroke-wi", DIAGRAM_REASON],
    ["<rect x=\"254\" y=\"22\" width=\"224\" height=\"64\" rx=\"8\" fill=\"#ffffff\"", DIAGRAM_REASON],
    ["<line x1=\"480\" y1=\"54\" x2=\"528\" y2=\"54\" stroke=\"#5b6774\" stroke-wi", DIAGRAM_REASON],
    ["<rect x=\"532\" y=\"22\" width=\"180\" height=\"64\" rx=\"8\" fill=\"#ffffff\"", DIAGRAM_REASON],
    ["<line x1=\"714\" y1=\"54\" x2=\"762\" y2=\"54\" stroke=\"#5b6774\" stroke-wi", DIAGRAM_REASON],
    ["<rect x=\"766\" y=\"22\" width=\"154\" height=\"64\" rx=\"8\" fill=\"#ffffff\"", DIAGRAM_REASON],
  ]),
  ...byFile('prompts/explain_template.html', [
    ["s multiply: 4 row counts × 7 radius bands × 3 offset groups = 84 f", PROSE_REASON],
    ["<svg viewBox=\"0 0 900 100\" width=\"100%\" role=\"img\" aria-label=\"Fra", DIAGRAM_REASON],
    ["<rect x=\"10\" y=\"20\" width=\"230\" height=\"60\" rx=\"7\" fill=\"#eef1f5\" ", DIAGRAM_REASON],
    ["<line x1=\"240\" y1=\"50\" x2=\"290\" y2=\"50\" stroke=\"#5b6774\"/>", DIAGRAM_REASON],
    ["<rect x=\"290\" y=\"20\" width=\"250\" height=\"60\" rx=\"7\" fill=\"#eaf5ec\"", DIAGRAM_REASON],
    ["<line x1=\"540\" y1=\"50\" x2=\"590\" y2=\"50\" stroke=\"#5b6774\"/>", DIAGRAM_REASON],
    ["<rect x=\"590\" y=\"20\" width=\"300\" height=\"60\" rx=\"7\" fill=\"#ffffff\"", DIAGRAM_REASON],
    ["<svg viewBox=\"0 0 900 100\" width=\"100%\" role=\"img\" aria-label=\"Pat", DIAGRAM_REASON],
    ["<rect x=\"10\" y=\"20\" width=\"160\" height=\"60\" rx=\"7\" fill=\"#eef1f5\" ", DIAGRAM_REASON],
    ["<line x1=\"170\" y1=\"50\" x2=\"200\" y2=\"50\" stroke=\"#5b6774\"/>", DIAGRAM_REASON],
    ["<rect x=\"200\" y=\"20\" width=\"170\" height=\"60\" rx=\"7\" fill=\"#ffffff\"", DIAGRAM_REASON],
    ["<line x1=\"370\" y1=\"50\" x2=\"400\" y2=\"50\" stroke=\"#5b6774\"/>", DIAGRAM_REASON],
    ["<rect x=\"400\" y=\"20\" width=\"170\" height=\"60\" rx=\"7\" fill=\"#ffffff\"", DIAGRAM_REASON],
    ["<line x1=\"570\" y1=\"50\" x2=\"600\" y2=\"50\" stroke=\"#5b6774\"/>", DIAGRAM_REASON],
    ["<rect x=\"600\" y=\"20\" width=\"170\" height=\"60\" rx=\"7\" fill=\"#eaf5ec\"", DIAGRAM_REASON],
  ]),
  ...byFile('prompts/plan_template.html', [
    ["n</span><span class=\"ar\"><svg class=\"cb-icon\" width=\"12\" height=\"1", PLAN_COPY_REASON],
    ["<path d=\"M5 12h14\" />", PLAN_COPY_REASON],
    ["<path d=\"m12 5 7 7-7 7\" />", PLAN_COPY_REASON],
    ["t</span><span class=\"ar\"><svg class=\"cb-icon\" width=\"12\" height=\"1", PLAN_COPY_REASON],
  ]),
  ...byFile('rust/crates/features/artifacts/src/artifact_check.rs', [
    ["let sentence = QUOTE.replace_all(sentence, \"「…」\").into_owned();", PROSE_REASON],
    ["r</span><span class=\"ar\">→</span><span class=\"b\">frame</span></div", FIXTURE_REASON],
    ["\"<div class=\\\"demo\\\"><svg viewBox=\\\"0 0 10 10\\\"><rect x=\\\"0\\\" y=\\\"", FIXTURE_REASON],
    ["r#\"<svg viewBox=\"0 0 100 40\"><rect x=\"0\" y=\"0\" width=\"30\" height=\"", FIXTURE_REASON],
    ["r#\"<line x1=\"30\" y1=\"15\" x2=\"60\" y2=\"15\"/><text x=\"62\" y=\"20\">guar", FIXTURE_REASON],
    ["r#\"<svg viewBox=\"0 0 10 10\"></svg>\"#,", FIXTURE_REASON],
    ["r#\"<svg viewBox=\"0 0 10 10\"><rect x=\"0\" y=\"0\" width=\"5\" height=\"5\"", FIXTURE_REASON],
  ]),
  ...byFile('rust/crates/features/artifacts/src/artifact_view.rs', [
    [" = inject_artifact_ui(\"<body>page</body>\", \"计划😀\", \"v1\", \"v2\", \"window.CB_ICON_TABLE = {};\");", TEST_DATA_REASON],
    ["ns(r#\"window.__cbcServerSessionId=\"\\u8ba1\\u5212\\ud83d\\ude00\";\"#));", TEST_DATA_REASON],
  ]),
  ...byFile('rust/crates/features/cron/src/loader.rs', [
    ["format!(\"⚠️ cron tasks failed to load: {}\"", TELEGRAM_ALERT_REASON],
    ["assert_eq!(*notifications.lock().unwrap(), [\"⚠️ cron tasks failed to load: alpha, beta\".to_string()]);", TELEGRAM_ALERT_REASON],
    ["\"⚠️ cron tasks failed to load: alpha, beta\".to_string(),", TELEGRAM_ALERT_REASON],
    ["\"⚠️ cron tasks failed to load: beta, gamma\".to_string(),", TELEGRAM_ALERT_REASON],
    ["\"⚠️ cron tasks failed to load: alpha, beta\".to_string(),", TELEGRAM_ALERT_REASON],
  ]),
  ...byFile('rust/crates/features/discord/src/discord_client/tests.rs', [
    ["3333333333333\", \"222222222222222222\", \"\\u{1F440}\").await.unwrap();", REACTION_REASON],
    ["3333333333333\", \"222222222222222222\", \"\\u{1F440}\").await.unwrap();", REACTION_REASON],
  ]),
  ...byFile('rust/crates/features/discord/src/discord_listener.rs', [
    ["const ACK_EMOJI: &str = \"👀\";", REACTION_REASON],
  ]),
  ...byFile('rust/crates/features/usage/src/pacific.rs', [
    ["Ok(format!(\"{} → {}\", self.monday, last_day))", PROSE_REASON],
    ["t.label().expect(\"the label formats\"), \"2026-10-19 → 2026-10-25\");", PROSE_REASON],
  ]),
  ...byFile('rust/crates/features/usage/src/page_memo.rs', [
    ["week: \"2026-11-02 → 2026-11-08\".to_string(),", PROSE_REASON],
  ]),
  ...byFile('rust/crates/features/usage/src/page_rows.rs', [
    ["is_empty() { String::new() } else { format!(\"{first} → {last}\") },", PROSE_REASON],
  ]),
  ...byFile('rust/crates/features/usage/src/page_tests.rs', [
    ["xt[\"ctx\"][\"rows\"][0][\"window\"], json!(\"2026-02-01 → 2026-02-09\"));", PROSE_REASON],
  ]),
  ...byFile('rust/crates/features/web_reply/src/reply.rs', [
    ["\"1. ▸ \\\"Keep the phone status toggle sticky while scrolling\\\"\\n\",", CHAT_TEXT_REASON],
    ["\"   ↳ 这一条为什么要改？先说明理由再动手。\"", CHAT_TEXT_REASON],
  ]),
  ...byFile('rust/crates/features/web_reply/src/status.rs', [
    ["us · Waiting on you]\\n1. ▸ Waiting on you · session {session_id} ·", CHAT_TEXT_REASON],
  ]),
  ...byFile('rust/crates/infra/compat/src/jinja.rs', [
    ["\"caf\\u{e9} \\u{2014} \\u{4e2d}\\u{6587} \\u{1f600}\",", TEST_DATA_REASON],
    ["\"\\u{4e2d}<a href=\\\"x\\\">&'\\u{1f600}\\\\", TEST_DATA_REASON],
  ]),
  ...byFile('rust/crates/infra/core/src/logline.rs', [
    ["'\\u{1f600}',", TEST_DATA_REASON],
  ]),
  ...byFile('rust/crates/infra/core/src/text_repr.rs', [
    [".chain(['\\u{1f600}', '\\u{7f}', '\\u{1}', '\\u{b}', '\\u{c}', '\"', '\\\\", TEST_DATA_REASON],
    ["\"caf\\u{e9} \\u{2014} \\u{4e2d}\\u{6587} \\u{1f600} \\u{10348}\",", TEST_DATA_REASON],
    ["\"\\u{0}a\\\"b\\\\c\\t\\u{e9}\\u{4e2d}\\u{1f600}~\",", TEST_DATA_REASON],
    ["\" back\\\\ nl\\newline\\r tab\\t del\\u{7f} \\u{e9} \\u{2014} \\u{1f600}\");", TEST_DATA_REASON],
    ["on!(\"raw \\u{e9} \\u{2014} \\u{1f600} \\u{1} \\u{b} del\\u{7f}\"), &STARL", TEST_DATA_REASON],
    ["\"\\\"raw \\u{e9} \\u{2014} \\u{1f600} \\\\u0001 \\\\u000b del\\u{7f}\\\"\"", TEST_DATA_REASON],
  ]),
  ...byFile('rust/crates/infra/html/src/entities.rs', [
    ["0x85 => \"\\u{2026}\".to_owned(),", TEST_DATA_REASON],
    ["0x9b => \"\\u{203a}\".to_owned(),", TEST_DATA_REASON],
  ]),
  ...byFile('rust/crates/runtime/registry/src/icons.rs', [
    ["_eq!(template.matches(r#\"<span class=\"ar\">→</span>\"#).count(), 0);", MODULE_REASON],
    ["\"<svg class=\\\"{class_name}\\\" width=\\\"{size}\\\" height=\\\"{size}\\\" vi", MODULE_REASON],
    ["source.find(\"<svg\").ok_or_else(|| IconError::new(format!(\"Lucide S", MODULE_REASON],
    ["\"Lucide SVG has an unfinished <svg> root: {}\", path.display())))?;", MODULE_REASON],
    ["tatic v1.76.0 - ISC -->\\n<svg viewBox=\\\"0 0 24 24\\\"><path d=\\\"M18 ", MODULE_REASON],
    ["tatic v1.76.0 - ISC -->\\n<svg viewBox=\\\"0 0 24 24\\\"><circle cx=\\\"1", MODULE_REASON],
  ]),
  ...byFile('rust/crates/runtime/registry/src/templating.rs', [
    ["tatic v1.76.0 - ISC -->\\n<svg viewBox=\\\"0 0 24 24\\\"><path d=\\\"M1 2", MODULE_REASON],
    ["tatic v1.76.0 - ISC -->\\n<svg viewBox=\\\"0 0 24 24\\\"><path d=\\\"M3 4", MODULE_REASON],
    ["tatic v1.76.0 - ISC -->\\n<svg viewBox=\\\"0 0 24 24\\\"><path d=\\\"M5 6", MODULE_REASON],
    ["assert!(changed.contains(\"<path d=\\\"M3 4\\\"/>\"));", MODULE_REASON],
  ]),
  ...byFile('rust/crates/runtime/store/src/aggregator.rs', [
    ["t!(\"Backend switched: {} → {}\", string_default(event, \"from\", \"\"),", PROSE_REASON],
    ["Subscription quota reset: switched {from_backend} → {to_backend}\")", PROSE_REASON],
    ["push_str(&format!(\" — {} → {} tokens\", format_k_tokens(pre_tokens)", PROSE_REASON],
  ]),
  ...byFile('rust/crates/runtime/store/src/fact_scan.rs', [
    ["us · Waiting on you]\\n1. ▸ Waiting on you · session synthetic · li", CHAT_TEXT_REASON],
    ["us · Waiting on you]\\n1. ▸ Waiting on you · session synthetic · li", CHAT_TEXT_REASON],
  ]),
  ...byFile('rust/crates/runtime/store/src/task_tree.rs', [
    ["erence)) = line.split_once(\". ▸ Waiting on you · session \") else {", CHAT_TEXT_REASON],
    ["\"{}. ▸ Waiting on you · session synthetic · list {:016x} · since 2", CHAT_TEXT_REASON],
    ["\"71. ▸ Waiting on you · session another-session · list fffffffffff", CHAT_TEXT_REASON],
    ["lines.push(\"72. ▸ Waiting on you · session synthetic · list invali", CHAT_TEXT_REASON],
    ["us · Waiting on you]\\n1. ▸ Waiting on you · session synthetic · li", CHAT_TEXT_REASON],
    ["us · Waiting on you]\\n1. ▸ Waiting on you · session synthetic · li", CHAT_TEXT_REASON],
  ]),
  ...byFile('rust/crates/runtime/tasks/src/handoff.rs', [
    ["format!(\"{}…\", line.chars().take(MAX_SUMMARY_LINE_CHARS).collect::", PROSE_REASON],
  ]),
  ...byFile('rust/crates/runtime/tasks/src/routes.rs', [
    ["us · Waiting on you]\\n1. ▸ Waiting on you · session synthetic-requ", CHAT_TEXT_REASON],
    ["us · Waiting on you]\\n1. ▸ Waiting on you · session synthetic-requ", CHAT_TEXT_REASON],
  ]),
  ...byFile('web/features/artifacts/static/artifact-comments.js', [
    ["return label ? ' \\u2192 ' + label : '';", PROSE_REASON],
    ["_HEAD_CHARS) head = head.slice(0, QUESTION_HEAD_CHARS) + '\\u2026';", PROSE_REASON],
    ["return heading ? heading + ' \\u203A ' + question : question;", BREADCRUMB_REASON],
    ["? context + ' \\u203A \"' + quote + '\"'", BREADCRUMB_REASON],
  ]),
  ...byFile('web/features/artifacts/static/plan-panel.js', [
    ["uestion.length > 40 ? question.slice(0, 40) + '\\u2026' : question;", PROSE_REASON],
  ]),
  ...byFile('web/features/code_server/static/open-in-code-server.js', [
    ["statusEl.textContent = 'preparing code-server…';", PROSE_REASON],
  ]),
  ...byFile('web/features/diff_view/static/diff_comments.js', [
    ["lines.push(`   ▸ \"${parts[0]}\"`);", CHAT_TEXT_REASON],
    ["lines.push(`   ▸ \"${parts[0]}`);", CHAT_TEXT_REASON],
    ["lines.push('   ↳ suggested replacement:');", CHAT_TEXT_REASON],
    ["lines.push(`   ↳ ${commentLines[0]}`);", CHAT_TEXT_REASON],
    ["return { text: 'Loading target session…', loading: true };", PROSE_REASON],
    ["pickerLoading ? { text: 'Loading sessions…', loading: true }", PROSE_REASON],
    ["const suffix = target ? ` → ${target.name}` : '';", PROSE_REASON],
  ]),
  ...byFile('web/features/diff_view/static/diff_page.js', [
    ["-center gap-2\">${cbIcon('loading', 18, 'spinner')}Loading…</div>`;", PROSE_REASON],
  ]),
  ...byFile('web/features/diff_view/templates/diff.html', [
    ["<span>Loading files…</span>", PROSE_REASON],
  ]),
  ...byFile('web/features/explain/static/explain.js', [
    ["\"explain-body text-slate-500 text-xs\">Loading explanation…</div>';", PROSE_REASON],
    ["if (pending) return 'Explaining with ' + escapeHtml(label) + '…';", PROSE_REASON],
    ["div class=\"text-slate-500 text-xs\">Generating explanation…</div>';", PROSE_REASON],
  ]),
  ...byFile('web/features/trace/templates/perfetto.html', [
    [" status_text = (phase ~ \"… 0s\") if phase else \"Downloading trace…\"", PROSE_REASON],
    ["updateStatus(`${phase}… ${Math.floor((Date.now() - started) / 1000", PROSE_REASON],
  ]),
  ...byFile('web/features/usage/static/ext_usage.js', [
    ["countEl.textContent = '×' + segment.count;", TIMES_REASON],
  ]),
  ...byFile('web/features/usage/templates/token_usage.html', [
    ["&middot; {{ window[0] }} &rarr; {{ window[1] }} {{ zone }}</span>{", PROSE_REASON],
  ]),
  ...byFile('web/features/voice/static/voice-input.js', [
    ["const VOICE_HINT_WAITING = 'Preparing microphone…';", PROSE_REASON],
  ]),
  ...byFile('web/features/voice/templates/voice/voice-events.js', [
    ["button.title = 'Preparing microphone…';", PROSE_REASON],
    ["button.setAttribute('aria-label', 'Preparing microphone…');", PROSE_REASON],
  ]),
  ...byFile('web/static/js/chat/rendering.js', [
    ["+ '<span id=\"' + id + '-short\"> &hellip; <button type=\"button\"'", PROSE_REASON],
    ["msg.delivery_status = 'Sending…';", PROSE_REASON],
  ]),
  ...byFile('web/static/js/chat/shared.js', [
    [" `<span id=\"${id}-short\">… <button onclick=\"document.getElementByI", PROSE_REASON],
    ["'none'?'block':'none'\" class=\"${buttonClass}\">Thinking…</button>`;", PROSE_REASON],
    ["if (compact) return pool.label + ' → ' + withoutFamilyPrefix(", PROSE_REASON],
    ["return pool.label + ' → ' + backendOptionLabel(poolMember) + (model", PROSE_REASON],
  ]),
  ...byFile('web/static/js/comment_post.js', [
    ["lines.push((i + 1) + '. ▸ ' + referenceLines[0]);", CHAT_TEXT_REASON],
    ["lines.push('   ↳ ' + answerLines[0]);", CHAT_TEXT_REASON],
  ]),
  ...byFile('web/static/js/concurrency-badge.js', [
    ["setStatus('Loading…');", PROSE_REASON],
    ["const axisTop = `<line class=\"cb-grid\" x1=\"1\" y1=\"${yAt(maximum)}\"", CHART_REASON],
    ["<svg width=\"${width}\" height=\"${height}\" viewBox=\"0 0 ${width} ${h", CHART_REASON],
    ["<line class=\"cb-grid\" x1=\"1\" y1=\"${yAt(0)}\" x2=\"${width - 1}\" y2=\"", CHART_REASON],
    ["<path class=\"cb-area\" d=\"${area}\"></path>", CHART_REASON],
    ["<path class=\"cb-line\" d=\"${segments.join('')}\"></path>", CHART_REASON],
    ["<line class=\"cb-cursor\" x1=\"0\" y1=\"${plotTop}\" x2=\"0\" y2=\"${plotBo", CHART_REASON],
    ["<circle class=\"cb-dot-mark\" cx=\"0\" cy=\"0\" r=\"3\" visibility=\"hidden", CHART_REASON],
  ]),
  ...byFile('web/static/js/icons.js', [
    ["return `<svg class=\"${className}\" width=\"${size}\" height=\"${size}\"", MODULE_REASON],
  ]),
  ...byFile('web/static/js/sidebar/filters.js', [
    ["renderSearchNote('Searching…');", PROSE_REASON],
  ]),
  ...byFile('web/static/js/sidebar/groups.js', [
    ["button.textContent = 'Loading…';", PROSE_REASON],
  ]),
  ...byFile('web/static/js/sidebar/modals.js', [
    ["nId, headHtml + taskContextNoteHtml('Loading next run context…'));", PROSE_REASON],
    ["if (bodyEl) bodyEl.innerHTML = taskContextNoteHtml('Loading…');", PROSE_REASON],
  ]),
  ...byFile('web/static/js/sidebar/session-menus.js', [
    ["items.push({label: 'Move to group…',", PROSE_REASON],
    ["? [{label: 'Move under…', onSelect: () => openSessionMovePicker(se", PROSE_REASON],
  ]),
  ...byFile('web/static/js/sidebar/session-move.js', [
    ["heading.textContent = 'Move under…';", PROSE_REASON],
  ]),
  ...byFile('web/static/js/sidebar/session-view.js', [
    ["abel.textContent = 'Opening ' + switchTargetName(sessionId) + '…';", PROSE_REASON],
    ["sentinel.innerHTML = 'Loading older messages&hellip;';", PROSE_REASON],
  ]),
  ...byFile('web/templates/events_viewer.html', [
    ["placeholder=\"e.g. user, assistant, result …\"", PROSE_REASON],
    ["Loading events…", PROSE_REASON],
    ["ength > PREVIEW_CHARS ? text.slice(0, PREVIEW_CHARS) + '…' : text;", PROSE_REASON],
    ["button.textContent = 'Loading…';", PROSE_REASON],
  ]),
];

function excused(finding) {
  return EXCEPTIONS.some((exception) => exception.file === finding.file && finding.source.includes(exception.text));
}

test('an icon is drawn only by the icon module', () => {
  const found = scanIconUses(ROOT)
    .filter((finding) => !excused(finding))
    .sort((a, b) => (a.file < b.file ? -1 : a.file > b.file ? 1 : a.line - b.line));
  const places = [];
  for (const finding of found) {
    const place = `${finding.file}:${finding.line}`;
    const last = places.at(-1);
    if (last && last.place === place) last.matches.push(finding.match);
    else places.push({ place, matches: [finding.match] });
  }
  const report = places.map(({ place, matches }) => {
    return `  ${place}  ${matches.map((match) => JSON.stringify(match)).join(', ')}`;
  });
  assert.equal(report.length, 0, failure(`${report.length} icon uses are drawn outside the icon module`, report));
});
