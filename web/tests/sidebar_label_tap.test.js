const assert = require('node:assert/strict');
const test = require('node:test');

const {loadGroups} = require('./sidebar_groups_context_stub');

// A row's model label: `.session-backend.truncate`, whose classes the tap toggles.
function labelElement({insideSidebar = true} = {}) {
  const classes = new Set(['session-backend', 'truncate']);
  return {
    insideSidebar,
    closest(selector) {
      return selector === '.session-backend' ? this : null;
    },
    classList: {
      has(name) {
        return classes.has(name);
      },
      toggle(name) {
        if (classes.has(name)) {
          classes.delete(name);
          return false;
        }
        classes.add(name);
        return true;
      },
    },
  };
}

function eventFor(target) {
  const calls = {prevented: 0, stopped: 0};
  return {
    target,
    preventDefault() {
      calls.prevented += 1;
    },
    stopPropagation() {
      calls.stopped += 1;
    },
    calls,
  };
}

// Registers the tap handler on a fake `#sidebar` and returns the listener as the page's mouse delivers it.
function tapListener(context, {matches = true} = {}) {
  const listeners = [];
  const sidebar = {
    addEventListener(type, listener, capture) {
      listeners.push({type, listener, capture});
    },
    contains(element) {
      return element !== null && element.insideSidebar !== false;
    },
  };
  context.document.getElementById = (id) => (id === 'sidebar' ? sidebar : null);
  context.window = {matchMedia: (query) => ({matches, query})};
  context.Sidebar.installSidebarLabelTap();
  assert.equal(listeners.length, 1, 'the sidebar holds one click listener');
  assert.equal(listeners[0].type, 'click');
  assert.equal(listeners[0].capture, true, 'the listener runs before the row onclick');
  return listeners[0].listener;
}

test('a phone tap on a row label expands it and does not switch sessions', () => {
  const context = loadGroups();
  const label = labelElement();
  const listener = tapListener(context);

  const first = eventFor(label);
  listener(first);
  assert.equal(label.classList.has('session-backend--full'), true);
  assert.equal(first.calls.prevented, 1, 'the row link does not navigate');
  assert.equal(first.calls.stopped, 1, 'the row onclick does not switch sessions');

  const second = eventFor(label);
  listener(second);
  assert.equal(label.classList.has('session-backend--full'), false);
});

test('a tap outside the label leaves the row switch to its own onclick', () => {
  const context = loadGroups();
  const listener = tapListener(context);
  const outside = eventFor({closest: () => null});
  listener(outside);
  assert.equal(outside.calls.prevented, 0);
  assert.equal(outside.calls.stopped, 0);
});

test('a label outside the sidebar is left alone', () => {
  const context = loadGroups();
  const label = labelElement({insideSidebar: false});
  const listener = tapListener(context);
  const event = eventFor(label);
  listener(event);
  assert.equal(label.classList.has('session-backend--full'), false);
  assert.equal(event.calls.stopped, 0);
});

test('a desktop click stays the hover title path', () => {
  const context = loadGroups();
  const label = labelElement();
  const listener = tapListener(context, {matches: false});
  const event = eventFor(label);
  listener(event);
  assert.equal(label.classList.has('session-backend--full'), false);
  assert.equal(event.calls.prevented, 0);
  assert.equal(event.calls.stopped, 0);
});
