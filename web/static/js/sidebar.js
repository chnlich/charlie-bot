// Compatibility loader for tests or tools that still execute sidebar.js directly.
// Runtime pages load the responsibility-scoped scripts under /static/js/sidebar/.
// compat-loader.js must run first in the same context.
charliebotLoadCompatModules('sidebar', [
  'namespace.js',
  'status.js',
  'workers.js',
  'filters.js',
  'groups.js',
  'session-view.js',
  'modals.js',
]);
