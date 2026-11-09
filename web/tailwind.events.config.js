// The Tailwind build the events viewer's own document carries.
//
// The events viewer inlines its stylesheet, and its markup and runtime script draw only a fraction of the utilities
// the chat page carries. Scanning just the files this page serves keeps the rest of `tailwind.config.js`'s utilities
// out of its document. A script the page starts to load must be added here, or its classes stay out of the build.
module.exports = {
  content: [
    './templates/events_viewer.html',
    './templates/partials/**/*.html',
    './templates/partials/**/*.js',
    './static/js/concurrency-badge.js',
  ],
  darkMode: 'class',
  theme: { extend: { colors: { slate: { 850: '#172033' } } } },
};
