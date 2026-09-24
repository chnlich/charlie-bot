
(function(global) {
  const Chat = global.Chat || {};

  function expose(names) {
    names.forEach((name) => { global[name] = Chat[name]; });
  }

  // Two disjoint lists: Object.assign puts both on Chat, and only globals'
  // keys become bare globals for the onclick strings and the cross-module
  // handlers that resolve bare names through window. Adding a member to
  // globals is enough for both; chatOnly stays reachable through Chat. A
  // module whose every member is bare-global passes one list only.
  function wire(globals, chatOnly) {
    Object.assign(Chat, globals, chatOnly || {});
    expose(Object.keys(globals));
  }

  Chat.wire = wire;
  global.Chat = Chat;
})(globalThis);
