// The Tailwind build the diff viewer's own document carries.
//
// web/features/diff_view/templates/diff.html links this sheet, and its markup,
// its two scripts and the diff toolbar page part draw only a fraction of the
// utilities the app page carries. Scanning just the files this page serves
// keeps the rest of tailwind.config.js's utilities out of its document. A
// script the page starts to load must be added here, or its classes stay out
// of the build.
module.exports = {
  content: [
    './features/diff_view/templates/**/*.html',
    './features/diff_view/static/**/*.js',
    './features/code_server/templates/**/*.html',
    './templates/partials/**/*.html',
    './static/js/comment_post.js',
    './static/js/concurrency-badge.js',
  ],
  darkMode: 'class',
  theme: { extend: { colors: { slate: { 850: '#172033' } } } },
};
