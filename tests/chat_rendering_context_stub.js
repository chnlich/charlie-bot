// Load core shared by the chat/rendering.js unit harnesses: namespace.js,
// shared.js, no-op stubs for rendering.js's two cross-module dependencies,
// then rendering.js. The stubs must land before rendering.js loads:
// rendering.js is an IIFE that binds Chat.renderRoundRatingButtons (defined in
// chat/ratings-recap.js) and Chat.embedLinkedHtmlArtifacts (chat/artifacts.js)
// into module-scope consts at load, and these harnesses load neither defining
// module. A harness that needs another module between shared.js and the stubs,
// or a non-empty rating-button stub, keeps its own sequence
// (tailwind_class_coverage.test.js).
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const NAMESPACE_JS = readStatic('chat/namespace.js');
const SHARED_JS = readStatic('chat/shared.js');
const RENDERING_JS = readStatic('chat/rendering.js');

function loadChatRenderingModules(context) {
  vm.runInContext(NAMESPACE_JS, context, {filename: 'chat/namespace.js'});
  vm.runInContext(SHARED_JS, context, {filename: 'chat/shared.js'});
  context.Chat.renderRoundRatingButtons = () => '';
  context.Chat.embedLinkedHtmlArtifacts = () => {};
  vm.runInContext(RENDERING_JS, context, {filename: 'chat/rendering.js'});
}

module.exports = {loadChatRenderingModules};
