// Compatibility loader for tests or tools that still execute chat.js directly.
// Runtime pages load the responsibility-scoped scripts under /static/js/chat/.
// compat-loader.js must run first in the same context; a direct CommonJS
// require loads it lazily instead.
if (typeof charliebotLoadCompatModules !== 'function' && typeof require === 'function') {
  require('./compat-loader.js');
}
charliebotLoadCompatModules('chat', [
  'namespace.js',
  'shared.js',
  'ratings-recap.js',
  'attachments.js',
  'artifacts.js',
  'scroll.js',
  'rendering.js',
  'turn-engine.js',
  'input.js',
]);
