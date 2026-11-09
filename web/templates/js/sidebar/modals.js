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
function initSidebarResize() {
  const sidebar = document.getElementById('sidebar');
  const handle = document.getElementById('resize-handle');
  const saved = localStorage.getItem('sidebar-width');
  if (saved) sidebar.style.width = saved + 'px';

  let startX, startW;
  handle.addEventListener('mousedown', (e) => {
    e.preventDefault();
    startX = e.clientX;
    startW = sidebar.offsetWidth;
    handle.classList.add('active');
    document.body.classList.add('resizing');

    function onMove(e) {
      const w = Math.min(Math.max(startW + e.clientX - startX, 200), 600);
      sidebar.style.width = w + 'px';
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
