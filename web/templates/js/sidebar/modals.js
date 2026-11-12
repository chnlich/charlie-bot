// ---------------------------------------------------------------------------
// Rename and session-action dialogs: the loader the document carries in place
// of the module.
// ---------------------------------------------------------------------------
// A reader reaches the dialogs only by clicking a rendered control, so the
// sidebar loads the interaction scripts together on the first control call.
// This file replaces the include of js/sidebar/modals.js, because the template
// loader reads web/templates before web/static and the page includes the same
// name.
//
// Two registrations must exist before the first transcript paints. The sidebar
// resize handle is not an interaction that can wait: the reader may drag it
// before any control call, and app.js initializes its wiring at load. The
// separator line's Clone to here and Elon-e buttons paint on the first render,
// to which a sidebar click never comes back. The module's copy used to register
// them at load, and core loads before every package, so these two lead the line.
// The filter strip's four pills end 294 px from the sidebar's left edge. With
// the strip's 12 px right padding, the sidebar needs 306 px for them. The New
// Session label stops clipping just above 283 px. Below those widths the
// Archive pill paints over the chat column and the label reads "N…", so a drag
// and a restored width both stop at 310 px.
const MIN_SIDEBAR_WIDTH = 310;
const MAX_SIDEBAR_WIDTH = 600;

function sidebarWidth(width) {
  return Math.min(Math.max(width, MIN_SIDEBAR_WIDTH), MAX_SIDEBAR_WIDTH);
}

function initSidebarResize() {
  const sidebar = document.getElementById('sidebar');
  const handle = document.getElementById('resize-handle');
  const saved = Number.parseInt(localStorage.getItem('sidebar-width'), 10);
  if (Number.isFinite(saved)) sidebar.style.width = sidebarWidth(saved) + 'px';

  let startX, startW;
  handle.addEventListener('mousedown', (e) => {
    e.preventDefault();
    startX = e.clientX;
    startW = sidebar.offsetWidth;
    handle.classList.add('active');
    document.body.classList.add('resizing');

    function onMove(e) {
      sidebar.style.width = sidebarWidth(startW + e.clientX - startX) + 'px';
    }
    function onUp() {
      handle.classList.remove('active');
      document.body.classList.remove('resizing');
      localStorage.setItem('sidebar-width', sidebar.offsetWidth);
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
    }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
}

Chat.registerSeparatorButton(function(sessionId, eventIndex) {
  return "<button onclick=\"forkSession(\x27" + sessionId + "\x27, " + eventIndex + ")\""
    + " class=\"p-0.5 text-slate-500 hover:text-green-400\" title=\"Clone to here\">"
    + Chat.CLONE_SVG
    + "</button>"
    + "<button onclick=\"eloneSession(\x27" + sessionId + "\x27, " + eventIndex + ")\""
    + " class=\"p-0.5 text-slate-500 hover:text-yellow-400\" title=\"Elon-e: retry with a fresh perspective\">"
    + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\"><path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M13 10V3L4 14h7v7l9-11h-7z\"/></svg>"
    + "</button>";
});

Sidebar.deferSidebarInteractions();
