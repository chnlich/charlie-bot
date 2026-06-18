var framed;
try { framed = window.parent !== window.self; } catch (e) { framed = true; }

if (!framed) {
  (function() {
    var GLOBAL_PREFIX = '__cbc';
    var BLOCK_SELECTOR = 'p,li,h1,h2,h3,h4,h5,h6,blockquote,pre,td,dd,section';
    var sessionId = extractSessionIdFromPath(window.location.pathname);
    var artifactPath = artifactPathFromPath(window.location.pathname);
    var hovered = null;
    var trigger = null;
    var popover = null;
    var toast = null;

    window.__cbcExtractSessionIdFromPath = extractSessionIdFromPath;

    installStyles();
    installListeners();

    function extractSessionIdFromPath(pathname) {
      var normalized = String(pathname || '').replace(/%2F/gi, '/');
      var match = normalized.match(/(?:^|\/)sessions\/([^/]+)\/artifacts(?:\/|$)/);
      if (!match) return null;
      var value = decodeURIComponent(match[1]).trim();
      return value || null;
    }

    function artifactPathFromPath(pathname) {
      return decodeURIComponent(String(pathname || '').replace(/%2F/gi, '/'));
    }

    function installStyles() {
      var style = document.createElement('style');
      style.textContent =
        '.' + GLOBAL_PREFIX + '-hover{outline:2px solid rgba(88,166,255,.95)!important;outline-offset:2px!important;box-shadow:0 0 0 4px rgba(88,166,255,.16)!important}' +
        '.' + GLOBAL_PREFIX + '-trigger{position:fixed;z-index:2147483646;width:28px;height:28px;border-radius:999px;border:1px solid rgba(88,166,255,.75);background:#0d1117;color:#e6edf3;display:none;align-items:center;justify-content:center;font-size:15px;line-height:1;box-shadow:0 8px 24px rgba(0,0,0,.35);cursor:pointer;padding:0}' +
        '.' + GLOBAL_PREFIX + '-trigger:hover{background:#1c2230;border-color:#58a6ff}' +
        '.' + GLOBAL_PREFIX + '-popover{position:fixed;z-index:2147483647;width:min(300px,calc(100vw - 16px));background:#161b22;color:#e6edf3;border:1px solid #2d3340;border-radius:8px;box-shadow:0 18px 50px rgba(0,0,0,.45);padding:10px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}' +
        '.' + GLOBAL_PREFIX + '-popover textarea{box-sizing:border-box;width:100%;min-height:72px;resize:vertical;background:#0d1117;color:#e6edf3;border:1px solid #2d3340;border-radius:6px;padding:7px 8px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;outline:none}' +
        '.' + GLOBAL_PREFIX + '-popover textarea:focus{border-color:#58a6ff;box-shadow:0 0 0 2px rgba(88,166,255,.18)}' +
        '.' + GLOBAL_PREFIX + '-actions{display:flex;justify-content:flex-end;gap:7px;margin-top:8px}' +
        '.' + GLOBAL_PREFIX + '-actions button{border:1px solid #2d3340;border-radius:6px;padding:4px 9px;background:#1c2230;color:#e6edf3;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-actions button:disabled{cursor:not-allowed;opacity:.58}' +
        '.' + GLOBAL_PREFIX + '-add{background:#238636!important;border-color:#2ea043!important;color:#fff!important}' +
        '.' + GLOBAL_PREFIX + '-error{display:none;margin-top:8px;color:#ffb4ab;font-size:12px}' +
        '.' + GLOBAL_PREFIX + '-toast{position:fixed;z-index:2147483647;max-width:min(360px,calc(100vw - 16px));left:50%;bottom:18px;transform:translateX(-50%);background:#1f6f3a;color:#dfffe5;border:1px solid rgba(63,185,80,.65);border-radius:999px;padding:7px 12px;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;box-shadow:0 10px 30px rgba(0,0,0,.35)}' +
        '.' + GLOBAL_PREFIX + '-toast-error{background:#5f2120;color:#ffe2df;border-color:rgba(248,81,73,.7);border-radius:8px}';
      document.head.appendChild(style);
    }

    function installListeners() {
      document.addEventListener('mouseover', function(event) {
        if (popover || isCommentUi(event.target)) return;
        var block = findBlock(event.target);
        if (block) setHovered(block);
      });
      document.addEventListener('mouseout', function(event) {
        if (popover) return;
        var related = event.relatedTarget;
        if (isCommentUi(related)) return;
        if (hovered && (!related || !hovered.contains(related))) clearHover();
      });
      document.addEventListener('mousemove', function() {
        if (hovered && !popover) positionTrigger(hovered);
      });
      window.addEventListener('scroll', reposition, true);
      window.addEventListener('resize', reposition);
    }

    function textFor(el, cap) {
      return (el.innerText || el.textContent || '').trim().slice(0, cap);
    }

    function cleanText(el, cap) {
      return textFor(el, cap).replace(/\s+/g, ' ');
    }

    function isTextBearing(el) {
      return Boolean(el && textFor(el, 1));
    }

    function isCommentUi(target) {
      return Boolean(target && target.closest && target.closest(
        '.' + GLOBAL_PREFIX + '-trigger,.' + GLOBAL_PREFIX + '-popover,.' + GLOBAL_PREFIX + '-toast'
      ));
    }

    function findBlock(target) {
      var el = target && target.closest ? target.closest(BLOCK_SELECTOR) : null;
      while (el && !isTextBearing(el)) {
        el = el.parentElement && el.parentElement.closest ? el.parentElement.closest(BLOCK_SELECTOR) : null;
      }
      return el;
    }

    function ensureTrigger() {
      if (trigger) return trigger;
      trigger = document.createElement('button');
      trigger.type = 'button';
      trigger.className = GLOBAL_PREFIX + '-trigger';
      trigger.textContent = '\uD83D\uDCAC';
      trigger.title = 'Comment on this block';
      trigger.setAttribute('aria-label', 'Comment on this block');
      trigger.addEventListener('mousedown', function(event) {
        event.preventDefault();
        event.stopPropagation();
      });
      trigger.addEventListener('click', function(event) {
        event.preventDefault();
        event.stopPropagation();
        if (hovered) openPopover(hovered);
      });
      document.body.appendChild(trigger);
      return trigger;
    }

    function positionTrigger(block) {
      var btn = ensureTrigger();
      var rect = block.getBoundingClientRect();
      btn.style.left = Math.max(8, Math.min(rect.right - 28, window.innerWidth - 36)) + 'px';
      btn.style.top = Math.max(8, Math.min(rect.top + 4, window.innerHeight - 36)) + 'px';
      btn.style.display = 'flex';
    }

    function setHovered(block) {
      if (hovered === block) {
        positionTrigger(block);
        return;
      }
      clearHover();
      hovered = block;
      hovered.classList.add(GLOBAL_PREFIX + '-hover');
      positionTrigger(block);
    }

    function clearHover() {
      if (hovered) hovered.classList.remove(GLOBAL_PREFIX + '-hover');
      hovered = null;
      if (trigger) trigger.style.display = 'none';
    }

    function previousHeading(block) {
      var headings = Array.prototype.slice.call(document.querySelectorAll('h1,h2,h3,h4,h5,h6'));
      var last = '';
      for (var i = 0; i < headings.length; i++) {
        var heading = headings[i];
        if (heading === block) break;
        if (heading.compareDocumentPosition(block) & Node.DOCUMENT_POSITION_FOLLOWING) {
          last = cleanText(heading, 200);
        }
      }
      return last;
    }

    function sectionTitle(block) {
      var tag = (block.tagName || '').toLowerCase();
      var section = tag === 'section' ? block : block.closest('section');
      if (!section) return '';
      var title = section.querySelector('h1,h2,h3,h4,h5,h6');
      if (!title || title === block) return '';
      if (block === section || (title.compareDocumentPosition(block) & Node.DOCUMENT_POSITION_FOLLOWING)) {
        return cleanText(title, 200);
      }
      return '';
    }

    function contextFor(block) {
      var tag = (block.tagName || '').toLowerCase();
      if (tag === 'section') return sectionTitle(block) || previousHeading(block);
      return previousHeading(block) || sectionTitle(block);
    }

    function closePopover() {
      if (popover && popover.parentNode) popover.parentNode.removeChild(popover);
      popover = null;
    }

    function positionPopover(node, block) {
      var rect = block.getBoundingClientRect();
      var width = Math.min(300, window.innerWidth - 16);
      var left = Math.max(8, Math.min(rect.left, window.innerWidth - width - 8));
      var top = Math.max(8, Math.min(rect.bottom + 8, window.innerHeight - node.offsetHeight - 8));
      node.style.left = left + 'px';
      node.style.top = top + 'px';
    }

    function setPopoverError(node, message) {
      var error = node.querySelector('.' + GLOBAL_PREFIX + '-error');
      error.textContent = message;
      error.style.display = 'block';
    }

    function showToast(message, isError) {
      if (toast && toast.parentNode) toast.parentNode.removeChild(toast);
      var node = document.createElement('div');
      toast = node;
      node.className = GLOBAL_PREFIX + '-toast' + (isError ? ' ' + GLOBAL_PREFIX + '-toast-error' : '');
      node.textContent = message;
      document.body.appendChild(node);
      window.setTimeout(function() {
        if (node.parentNode) node.parentNode.removeChild(node);
        if (toast === node) toast = null;
      }, 2200);
    }

    function buildMessage(block, comment) {
      var blockText = textFor(block, 400);
      var context = contextFor(block);
      var quoteLine = context
        ? '\u25B8 ' + context + ' \u203A "' + blockText + '"'
        : '\u25B8 "' + blockText + '"';
      return '[Artifact comment \u00B7 ' + artifactPath + ']\n' +
        quoteLine + '\n' +
        '\u21B3 ' + comment;
    }

    function openPopover(block) {
      closePopover();
      if (trigger) trigger.style.display = 'none';
      hovered = block;
      hovered.classList.add(GLOBAL_PREFIX + '-hover');

      var node = document.createElement('div');
      node.className = GLOBAL_PREFIX + '-popover';
      var textarea = document.createElement('textarea');
      textarea.placeholder = 'Add a comment';
      var error = document.createElement('div');
      error.className = GLOBAL_PREFIX + '-error';
      var actions = document.createElement('div');
      actions.className = GLOBAL_PREFIX + '-actions';
      var addBtn = document.createElement('button');
      addBtn.type = 'button';
      addBtn.className = GLOBAL_PREFIX + '-add';
      addBtn.textContent = 'Add';
      var cancelBtn = document.createElement('button');
      cancelBtn.type = 'button';
      cancelBtn.textContent = 'Cancel';

      actions.appendChild(cancelBtn);
      actions.appendChild(addBtn);
      node.appendChild(textarea);
      node.appendChild(error);
      node.appendChild(actions);
      node.addEventListener('mousedown', function(event) { event.stopPropagation(); });
      cancelBtn.addEventListener('click', function() {
        closePopover();
        clearHover();
      });

      if (!sessionId) {
        addBtn.disabled = true;
        setPopoverError(node, 'Cannot parse session id from this artifact URL.');
      } else {
        addBtn.addEventListener('click', function() {
          submitComment(node, addBtn, block, textarea);
        });
      }

      document.body.appendChild(node);
      popover = node;
      positionPopover(node, block);
      textarea.focus();
    }

    async function submitComment(node, addBtn, block, textarea) {
      var comment = textarea.value.trim();
      if (!comment) {
        textarea.focus();
        return;
      }
      addBtn.disabled = true;
      addBtn.textContent = 'Sending';
      try {
        var response = await fetch('/api/chat/' + encodeURIComponent(sessionId) + '/message', {
          method: 'POST',
          headers: {'Content-Type': 'application/json'},
          credentials: 'same-origin',
          body: JSON.stringify({content: buildMessage(block, comment), uploaded_files: []}),
        });
        if (response.status === 401) {
          setPopoverError(node, 'log in to CharlieBot to comment');
          showToast('log in to CharlieBot to comment', true);
          return;
        }
        if (!response.ok) {
          throw new Error('Comment post failed: HTTP ' + response.status);
        }
        closePopover();
        clearHover();
        showToast('Comment sent to chat', false);
      } catch (err) {
        console.error('Artifact comment failed:', err);
        setPopoverError(node, err.message);
        showToast(err.message, true);
      } finally {
        if (popover === node) {
          addBtn.disabled = false;
          addBtn.textContent = 'Add';
        }
      }
    }

    function reposition() {
      if (!hovered) return;
      if (popover) positionPopover(popover, hovered);
      else positionTrigger(hovered);
    }
  })();
}
