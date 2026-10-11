// The delivery banner's Diff link: the diff page compares the run's work branch
// against its base branch.
function deliveryDiffHref(msg, sessionId) {
  if (!msg || !msg.repo_path || !msg.base_branch || !msg.branch_name) return null;
  return '/diff?repo=' + encodeURIComponent(msg.repo_path)
    + '&base=' + encodeURIComponent(msg.base_branch)
    + '&head=' + encodeURIComponent(msg.branch_name)
    + '&session=' + encodeURIComponent(sessionId || '');
}

Chat.registerDeliveryLink({label: 'Diff', href: deliveryDiffHref});
