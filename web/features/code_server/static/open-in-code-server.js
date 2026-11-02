// The diff page's "Open in code-server" button: the server starts code-server on
// the repository the repo field names, and the page opens it in a new tab.
// Progress shows in the diff page's #status line and a failure in its #error
// box, the two elements the diff page reports through. The script loads
// deferred, so every element it reads exists when it runs.
(() => {
  const button = document.getElementById('open-codeserver');
  const repoInput = document.getElementById('repo-input');
  const statusEl = document.getElementById('status');
  const errorEl = document.getElementById('error');

  function setError(msg) {
    errorEl.textContent = msg;
    errorEl.classList.remove('hidden');
  }

  function clearError() { errorEl.classList.add('hidden'); errorEl.textContent = ''; }

  function responseDetail(data, status) {
    const detail = (data && (data.detail?.message || data.detail)) || `HTTP ${status}`;
    return typeof detail === 'string' ? detail : JSON.stringify(detail);
  }

  async function openCodeServer() {
    clearError();
    const repo = repoInput.value.trim();
    if (!repo) {
      setError('repo is required to open code-server');
      return;
    }
    statusEl.textContent = 'preparing code-server…';

    let resp;
    try {
      resp = await fetch(`/api/code-server/open?folder=${encodeURIComponent(repo)}`);
    } catch (e) {
      statusEl.textContent = '';
      setError(`Network error: ${e.message}`);
      return;
    }
    let data;
    try {
      data = await resp.json();
    } catch (e) {
      statusEl.textContent = '';
      setError(`Server returned non-JSON response (HTTP ${resp.status})`);
      return;
    }
    statusEl.textContent = '';

    if (!resp.ok) {
      setError(responseDetail(data, resp.status));
      return;
    }

    const url = `${location.protocol}//${location.hostname}:${data.port}/?folder=${encodeURIComponent(data.folder)}`;
    window.open(url, '_blank');
  }

  button.addEventListener('click', openCodeServer);
})();
