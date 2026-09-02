// ---------------------------------------------------------------------------
// Shared .prose-msg DOM stub for the node --test vm render tests (no jsdom).
// Both chat_artifact_cards.test.js and chat_attachments_render.test.js drive
// chat/artifacts.js through the same anchor/prose/parent surface; a member the
// script reads but the stub lacks throws inside the vm and fails the test.
// ---------------------------------------------------------------------------

function makeAnchor(href) {
  return {
    dataset: {},
    isConnected: true,
    href,
    getAttribute(name) {
      return name === 'href' ? this.href : null;
    },
    setAttribute(name, value) {
      if (name === 'href') this.href = value;
    },
    closest(selector) {
      return selector === '.prose-msg' ? this.prose : null;
    },
  };
}

// opts: {id, anchors, codes, childNodes} — every member defaults to empty so
// the prose root covers both the anchor-only and the child-walking scenarios.
function makeProseRoot(opts) {
  opts = opts || {};
  const parent = {
    inserted: [],
    insertBefore(child) {
      this.inserted.push(child);
      child.parentNode = this;
      return child;
    },
  };
  const prose = {
    id: opts.id || '',
    isConnected: true,
    nodeType: 1,
    tagName: 'DIV',
    childNodes: opts.childNodes || [],
    parentNode: parent,
    nextSibling: null,
    nextElementSibling: null,
    closest() {
      return null;
    },
    querySelectorAll(selector) {
      if (selector === 'a[href]') return opts.anchors || [];
      if (selector === 'code') return opts.codes || [];
      return [];
    },
  };
  for (const el of [...(opts.anchors || []), ...(opts.codes || [])]) el.prose = prose;
  return {
    parent,
    prose,
    root: {
      querySelectorAll(selector) {
        return selector === '.prose-msg' ? [prose] : [];
      },
    },
  };
}

module.exports = { makeAnchor, makeProseRoot };
