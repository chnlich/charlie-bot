// ---------------------------------------------------------------------------
// The minimal hljs surface markdown-renderer.js touches at load, for the vm
// harnesses that run it outside a browser. highlight/highlightAuto return the
// input unchanged, so the fake highlights nothing; a harness that must count
// or shape calls builds its own stand-in instead.
// ---------------------------------------------------------------------------
const hljsStub = {
  getLanguage: () => null,
  highlightAuto: (s) => ({ value: String(s) }),
  highlight: (s) => ({ value: String(s) }),
};

module.exports = { hljsStub };
