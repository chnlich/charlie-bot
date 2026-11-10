// The Tailwind build the app page's document carries.
//
// web/templates/index.html inlines this sheet, so it carries every utility that
// page can paint: its own markup and partials, the core scripts, and each page
// package's page part templates and static modules the page loads on use. A
// package file the app page never renders is left out, even when another page
// of the same package uses it: the diff toolbar part and the code-server
// scripts belong to the diff viewer's sheet (tailwind.diff.config.js), and the
// standalone pages' own templates build no sheet of this one.
module.exports = {
  content: [
    './templates/index.html',
    './templates/partials/**/*.html',
    './static/js/**/*.js',
    './features/artifacts/templates/artifacts/page_*.html',
    './features/artifacts/static/**/*.js',
    './features/chat_threads/templates/chat_threads/page_*.html',
    './features/chat_threads/static/**/*.js',
    './features/cron/templates/cron/page_*.html',
    './features/cron/static/**/*.js',
    './features/diag/templates/diag/page_*.html',
    './features/diag/templates/**/*.js',
    './features/diff_view/templates/diff_view/page_*.html',
    './features/diff_view/templates/**/*.js',
    './features/drafts/templates/drafts/page_*.html',
    './features/drafts/static/**/*.js',
    './features/explain/templates/explain/page_*.html',
    './features/explain/static/**/*.js',
    './features/terminal/templates/terminal/page_*.html',
    './features/terminal/static/**/*.js',
    './features/usage/templates/usage/page_*.html',
    './features/usage/static/**/*.js',
    './features/voice/templates/voice/page_*.html',
    './features/voice/templates/voice/voice-events.js',
    './features/voice/static/**/*.js',
    './backends/*/templates/**/*.html',
    './backends/*/static/**/*.js',
  ],
  darkMode: 'class',
  theme: { extend: { colors: { slate: { 850: '#172033' } } } },
};
