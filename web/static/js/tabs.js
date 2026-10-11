// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------
// Each tab registers its id, its layout and its callbacks. showsChat keeps the
// chat column beside the tab's panel; fullScreenOnMobile hides the chat column
// on a phone; keepOnSessionSwitch keeps the tab open when the user opens
// another session; onShow and onHide run on every switch, after the chat
// column is laid out. A package registers its tab when its script loads, and
// its tab button (id btn-<id>) is one of its page parts.
const registeredTabs = new Map();
let activeTabId = 'chat';

function registerTab(tab) {
  if (!tab || typeof tab.id !== 'string' || !tab.id || typeof tab.showsChat !== 'boolean'
      || typeof tab.fullScreenOnMobile !== 'boolean' || typeof tab.keepOnSessionSwitch !== 'boolean'
      || typeof tab.onShow !== 'function' || typeof tab.onHide !== 'function') {
    throw new Error('Invalid tab registration');
  }
  if (registeredTabs.has(tab.id)) throw new Error('Duplicate tab: ' + tab.id);
  registeredTabs.set(tab.id, tab);
}

registerTab({
  id: 'chat',
  showsChat: true,
  fullScreenOnMobile: false,
  keepOnSessionSwitch: true,
  onShow() {},
  onHide() {},
});

function switchTab(tab) {
  const shown = registeredTabs.get(tab);
  if (!shown) throw new Error('Unknown tab: ' + tab);
  // A worker node renders its Run transcript through the same chat column
  // (session-view.js), so no leaf swap exists.
  const chatEl = document.getElementById('tab-chat');
  chatEl.classList.toggle('hidden', !shown.showsChat);
  if (platform.isMobile && shown.fullScreenOnMobile) chatEl.classList.add('hidden');

  registeredTabs.forEach((entry, id) => { if (id !== tab) entry.onHide(); });
  shown.onShow();
  activeTabId = tab;

  // Highlight active tab button
  registeredTabs.forEach((entry, id) => {
    const btn = document.getElementById('btn-' + id);
    if (!btn) return;
    if (id === tab) {
      btn.classList.add('bg-blue-600/20', 'text-blue-300');
      btn.classList.remove('text-slate-400');
    } else {
      btn.classList.remove('bg-blue-600/20', 'text-blue-300');
      btn.classList.add('text-slate-400');
    }
  });
}

// The tab to show after the user opens another session: the open tab when it
// keeps across a session switch, otherwise chat.
function tabAfterSessionSwitch() {
  return registeredTabs.get(activeTabId).keepOnSessionSwitch ? activeTabId : 'chat';
}

// ---------------------------------------------------------------------------
// Chat input
// ---------------------------------------------------------------------------
function autoResize(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(Math.max(el.scrollHeight, 38), 200) + 'px';
}
