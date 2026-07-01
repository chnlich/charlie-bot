
(function(global) {
  const Chat = global.Chat || {};

  function expose(names) {
    names.forEach((name) => { global[name] = Chat[name]; });
  }

  Chat.expose = expose;
  global.Chat = Chat;
})(globalThis);
