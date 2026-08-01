
(function(global) {
  const Chat = global.Chat || {};
  const state = Chat.state || {};
  Chat.state = state;

  function expose(names) {
    names.forEach((name) => { global[name] = Chat[name]; });
  }

  function exposeState(name, value) {
    if (!(name in state)) state[name] = typeof global[name] === 'undefined' ? value : global[name];
    Object.defineProperty(global, name, {
      configurable: true,
      get() { return state[name]; },
      set(v) { state[name] = v; },
    });
  }

  Chat.expose = expose;
  Chat.exposeState = exposeState;
  global.Chat = Chat;
})(globalThis);
