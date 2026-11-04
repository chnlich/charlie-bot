// Session-switch diagnostics: each switch phase core announces goes to the
// server's switch-event log. Fire-and-forget by design: never awaited, failures
// dropped, so the diagnostics channel never delays or breaks a switch.
document.addEventListener('charliebot:session-switch-phase', (event) => {
  fetch('/api/diag/switch-events', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(event.detail),
  }).catch(() => {});
});
