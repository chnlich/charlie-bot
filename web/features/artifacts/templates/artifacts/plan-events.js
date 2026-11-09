// The Plans tab's plan_updated subscription. It lives with the page rather than the panel's script. The page loads
// that script on demand, and an update that arrives first must still raise the tab's new-plan dot. The first
// update loads the panel and then runs the panel's own handler, which fetches the registry and marks the dot; a
// loaded panel takes the call directly.
function handlePlanUpdatedEvent(event) {
  if (typeof planPanel !== 'undefined' && planPanel && typeof planPanel.onPlanUpdated === 'function') {
    planPanel.onPlanUpdated(event.plan_id);
    return;
  }
  loadPlansPanel().then(function() {
    planPanel.onPlanUpdated(event.plan_id);
  }).catch((error) => console.error(error));
}
registerWsMessageHandler('plan_updated', handlePlanUpdatedEvent, {advancesCursor: true});
