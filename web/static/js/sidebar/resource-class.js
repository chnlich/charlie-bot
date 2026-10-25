// ---------------------------------------------------------------------------
// The session page's repair mark (sidebar/resource-class.js): the header toggle
// that moves a session and, through it, its descendants between the agents'
// memory share and the repair share. Only an operator credential may call the
// route, so an agent cannot move itself out of the agents' share.
// ---------------------------------------------------------------------------
function setRepairShareState(resourceClass) {
  const button = document.getElementById('resource-class-toggle');
  if (!button) return;
  const repair = resourceClass === 'repair';
  button.dataset.resourceClass = repair ? 'repair' : 'agent';
  button.setAttribute('aria-pressed', repair ? 'true' : 'false');
  button.textContent = 'Reserved memory: ' + (repair ? 'on' : 'off');
}

async function toggleRepairShare() {
  const button = document.getElementById('resource-class-toggle');
  if (!button || !SESSION_ID) return;
  const next = button.dataset.resourceClass === 'repair' ? 'agent' : 'repair';
  button.disabled = true;
  try {
    const response = await fetch('/api/sessions/' + encodeURIComponent(SESSION_ID) + '/resource-class', {
      method: 'PUT',
      headers: JSON_HEADERS,
      body: JSON.stringify({resource_class: next}),
    });
    if (!response.ok) {
      alert('Reserved memory change failed: ' + response.status + ' ' + (await response.text()));
      return;
    }
    const result = await response.json();
    setRepairShareState(result.resource_class);
  } finally {
    button.disabled = false;
  }
}
