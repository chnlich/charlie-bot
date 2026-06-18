var framed;
try { framed = window.parent !== window.self; } catch (e) { framed = true; }

if (!framed) {
  (function() {
    var GLOBAL_PREFIX = '__cbc';
    var HIDE_DELAY_MS = 300;
    var TRIGGER_SIZE = 34;
    var AUTH_MESSAGE = 'log in to comment';
    var BLOCK_SELECTOR = 'p,li,h1,h2,h3,h4,h5,h6,blockquote,pre,td,dd';
    var SECTION_SELECTOR = 'section';
    var SHORTCUTS = [{label: 'Improve', prompt: 'Think from scratch, how to improve this plan?'}];
    var sessionId = extractSessionIdFromPath(window.location.pathname);
    var artifactPath = artifactPathFromPath(window.location.pathname);
    var hovered = null;
    var hideTimer = null;
    var trigger = null;
    var popover = null;
    var toast = null;
    var pending = [];
    var tray = null;
    var trayHeader = null;
    var trayList = null;
    var traySendBtn = null;
    var trayClearBtn = null;
    var trayReason = null;

    window.__cbcExtractSessionIdFromPath = extractSessionIdFromPath;
    window.__cbcBuildBatchMessage = buildBatchMessage;

    installStyles();
    installListeners();
    installShortcuts();
    installBatchTray();

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
        '.' + GLOBAL_PREFIX + '-trigger{position:fixed;z-index:2147483646;width:34px;height:34px;border-radius:999px;border:1px solid rgba(88,166,255,.75);background:#0d1117;color:#e6edf3;display:none;align-items:center;justify-content:center;font-size:16px;line-height:1;box-shadow:0 8px 24px rgba(0,0,0,.35);cursor:pointer;padding:0}' +
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
        '.' + GLOBAL_PREFIX + '-toast-error{background:#5f2120;color:#ffe2df;border-color:rgba(248,81,73,.7);border-radius:8px}' +
        '.' + GLOBAL_PREFIX + '-shortcuts{position:fixed;right:14px;bottom:64px;z-index:2147483646;display:flex;flex-direction:column;align-items:flex-end;gap:6px;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}' +
        '.' + GLOBAL_PREFIX + '-shortcut{border:1px solid #2ea043;border-radius:7px;padding:7px 10px;background:#238636;color:#fff;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;box-shadow:0 10px 30px rgba(0,0,0,.35);cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-shortcut:hover:not(:disabled){background:#2ea043}' +
        '.' + GLOBAL_PREFIX + '-shortcut:disabled{cursor:not-allowed;opacity:.64;background:#30363d;border-color:#484f58;color:#c9d1d9}' +
        '.' + GLOBAL_PREFIX + '-shortcut-reason{max-width:220px;background:#5f2120;color:#ffe2df;border:1px solid rgba(248,81,73,.7);border-radius:6px;padding:5px 7px;line-height:1.3;box-shadow:0 10px 30px rgba(0,0,0,.35)}' +
        '.' + GLOBAL_PREFIX + '-marked{outline:2px solid rgba(88,166,255,.45)!important;outline-offset:2px!important;box-shadow:0 0 0 4px rgba(88,166,255,.08)!important}' +
        '.' + GLOBAL_PREFIX + '-tray{position:fixed;right:14px;bottom:110px;z-index:2147483646;width:min(300px,calc(100vw - 28px));background:#161b22;color:#e6edf3;border:1px solid #2d3340;border-radius:8px;box-shadow:0 18px 50px rgba(0,0,0,.45);padding:10px;font:13px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;display:none;flex-direction:column;gap:8px}' +
        '.' + GLOBAL_PREFIX + '-tray-header{font-weight:600;font-size:12px;color:#8b949e}' +
        '.' + GLOBAL_PREFIX + '-tray-list{max-height:200px;overflow:auto;display:flex;flex-direction:column;gap:6px}' +
        '.' + GLOBAL_PREFIX + '-tray-item{position:relative;background:#0d1117;border:1px solid #2d3340;border-radius:6px;padding:7px 28px 7px 8px}' +
        '.' + GLOBAL_PREFIX + '-tray-item-quote{color:#8b949e;font-size:11px;font-style:italic;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}' +
        '.' + GLOBAL_PREFIX + '-tray-item-comment{color:#e6edf3;font-size:12px;margin-top:3px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}' +
        '.' + GLOBAL_PREFIX + '-tray-remove{position:absolute;top:4px;right:4px;width:18px;height:18px;border:none;border-radius:4px;background:transparent;color:#8b949e;font:14px/1 -apple-system,BlinkMacSystemFont,sans-serif;cursor:pointer;padding:0;display:flex;align-items:center;justify-content:center}' +
        '.' + GLOBAL_PREFIX + '-tray-remove:hover{background:#21262d;color:#e6edf3}' +
        '.' + GLOBAL_PREFIX + '-tray-reason{background:#5f2120;color:#ffe2df;border:1px solid rgba(248,81,73,.7);border-radius:6px;padding:5px 7px;font-size:11px;line-height:1.3}' +
        '.' + GLOBAL_PREFIX + '-tray-actions{display:flex;justify-content:flex-end;gap:7px}' +
        '.' + GLOBAL_PREFIX + '-tray-send{border:1px solid #2ea043;border-radius:6px;padding:5px 10px;background:#238636;color:#fff;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-tray-send:hover:not(:disabled){background:#2ea043}' +
        '.' + GLOBAL_PREFIX + '-tray-send:disabled{cursor:not-allowed;opacity:.64;background:#30363d;border-color:#484f58;color:#c9d1d9}' +
        '.' + GLOBAL_PREFIX + '-tray-clear{border:1px solid #2d3340;border-radius:6px;padding:5px 10px;background:#1c2230;color:#e6edf3;font:12px -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;cursor:pointer}' +
        '.' + GLOBAL_PREFIX + '-tray-clear:hover{background:#262d36}';
      document.head.appendChild(style);
    }

    function installListeners() {
      document.addEventListener('mouseover', function(event) {
        if (isCommentUi(event.target)) {
          cancelHide();
          return;
        }
        if (popover) return;
        var block = findBlock(event.target);
        if (block) {
          cancelHide();
          setHovered(block);
        }
      });
      document.addEventListener('mouseout', function(event) {
        if (popover) return;
        var related = event.relatedTarget;
        if (isCommentUi(related)) {
          cancelHide();
          return;
        }
        if (hovered && (!related || !hovered.contains(related))) scheduleHide();
      });
      document.addEventListener('click', function(event) {
        if (isCommentUi(event.target) || isInteractiveElement(event.target) || hasActiveTextSelection()) return;
        var block = findBlock(event.target);
        if (block) openPopover(block);
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
        '.' + GLOBAL_PREFIX + '-trigger,.' + GLOBAL_PREFIX + '-popover,.' + GLOBAL_PREFIX + '-toast,.' + GLOBAL_PREFIX + '-tray'
      ));
    }

    function isInteractiveElement(target) {
      return Boolean(target && target.closest && target.closest('a,button,input,textarea,select,label'));
    }

    function hasActiveTextSelection() {
      var selection = window.getSelection && window.getSelection();
      return Boolean(selection && !selection.isCollapsed);
    }

    function findBlock(target) {
      var el = target && target.closest ? target.closest(BLOCK_SELECTOR) : null;
      while (el && !isTextBearing(el)) {
        el = el.parentElement && el.parentElement.closest ? el.parentElement.closest(BLOCK_SELECTOR) : null;
      }
      if (el) return el;

      el = target && target.closest ? target.closest(SECTION_SELECTOR) : null;
      while (el && !isTextBearing(el)) {
        el = el.parentElement && el.parentElement.closest ? el.parentElement.closest(SECTION_SELECTOR) : null;
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
      trigger.addEventListener('mouseenter', cancelHide);
      trigger.addEventListener('mouseleave', function() {
        if (!popover) scheduleHide();
      });
      document.body.appendChild(trigger);
      return trigger;
    }

    function positionTrigger(block) {
      var btn = ensureTrigger();
      var rect = block.getBoundingClientRect();
      btn.style.left = Math.max(8, Math.min(rect.right - TRIGGER_SIZE, window.innerWidth - TRIGGER_SIZE - 8)) + 'px';
      btn.style.top = Math.max(8, Math.min(rect.top + 4, window.innerHeight - TRIGGER_SIZE - 8)) + 'px';
      btn.style.display = 'flex';
    }

    function setHovered(block) {
      cancelHide();
      if (hovered === block) {
        positionTrigger(block);
        return;
      }
      clearHover();
      hovered = block;
      hovered.classList.add(GLOBAL_PREFIX + '-hover');
      positionTrigger(block);
    }

    function scheduleHide() {
      cancelHide();
      hideTimer = window.setTimeout(function() {
        hideTimer = null;
        clearHover();
      }, HIDE_DELAY_MS);
    }

    function cancelHide() {
      if (!hideTimer) return;
      window.clearTimeout(hideTimer);
      hideTimer = null;
    }

    function clearHover() {
      cancelHide();
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

    function buildCommentEntry(quote, context, comment) {
      var quoteLine = context
        ? '\u25B8 ' + context + ' \u203A "' + quote + '"'
        : '\u25B8 "' + quote + '"';
      return quoteLine.split('\n').concat(('\u21B3 ' + comment).split('\n'));
    }

    function buildBatchMessage(entries) {
      var lines = [];
      lines.push('[Artifact comments \u00B7 ' + artifactPath + '] (' + entries.length + ')');
      lines.push('');
      for (var i = 0; i < entries.length; i++) {
        var entry = entries[i];
        var entryLines = buildCommentEntry(entry.quote, entry.context, entry.comment);
        lines.push((i + 1) + '. ' + entryLines[0]);
        for (var j = 1; j < entryLines.length; j++) {
          lines.push('   ' + entryLines[j]);
        }
        if (i < entries.length - 1) lines.push('');
      }
      return lines.join('\n');
    }

    function buildShortcutMessage(shortcut) {
      return '[Plan \u00B7 ' + artifactPath + ']\n' + shortcut.prompt;
    }

    function installShortcuts() {
      var container = document.createElement('div');
      container.className = GLOBAL_PREFIX + '-shortcuts';

      for (var i = 0; i < SHORTCUTS.length; i++) {
        (function(shortcut) {
          var button = document.createElement('button');
          button.type = 'button';
          button.className = GLOBAL_PREFIX + '-shortcut';
          button.textContent = shortcut.label;
          button.title = shortcut.prompt;
          button.setAttribute('aria-label', shortcut.prompt);
          if (!sessionId) {
            button.disabled = true;
          } else {
            button.addEventListener('click', function() {
              sendShortcut(shortcut, button);
            });
          }
          container.appendChild(button);
        })(SHORTCUTS[i]);
      }

      if (!sessionId) {
        var reason = document.createElement('div');
        reason.className = GLOBAL_PREFIX + '-shortcut-reason';
        reason.textContent = 'Cannot parse session id from this artifact URL.';
        container.appendChild(reason);
      }

      document.body.appendChild(container);
    }

    function openPopover(block) {
      cancelHide();
      closePopover();
      if (hovered && hovered !== block) clearHover();
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
      node.addEventListener('mouseenter', cancelHide);
      cancelBtn.addEventListener('click', function() {
        closePopover();
        clearHover();
      });

      if (!sessionId) {
        addBtn.disabled = true;
        setPopoverError(node, 'Cannot parse session id from this artifact URL.');
      } else {
        addBtn.addEventListener('click', function() {
          submitComment(block, textarea);
        });
      }

      document.body.appendChild(node);
      popover = node;
      positionPopover(node, block);
      textarea.focus();
    }

    async function postChatMessage(content) {
      var response = await fetch('/api/chat/' + encodeURIComponent(sessionId) + '/message', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        credentials: 'same-origin',
        body: JSON.stringify({content: content, uploaded_files: []}),
      });
      if (response.status === 401) {
        throw new Error(AUTH_MESSAGE);
      }
      if (!response.ok) {
        throw new Error('Comment post failed: HTTP ' + response.status);
      }
    }

    async function sendShortcut(shortcut, button) {
      var label = button.textContent;
      button.disabled = true;
      button.textContent = 'Sending';
      try {
        await postChatMessage(buildShortcutMessage(shortcut));
        showToast('Sent to chat', false);
      } catch (err) {
        console.error('Artifact shortcut failed:', err);
        showToast(err.message, true);
      } finally {
        button.disabled = false;
        button.textContent = label;
      }
    }

    function submitComment(block, textarea) {
      var comment = textarea.value.trim();
      if (!comment) {
        textarea.focus();
        return;
      }
      pending.push({
        el: block,
        quote: cleanText(block, 400),
        context: contextFor(block),
        comment: comment,
      });
      block.classList.add(GLOBAL_PREFIX + '-marked');
      closePopover();
      clearHover();
      refreshTray();
    }

    function installBatchTray() {
      var container = document.createElement('div');
      container.className = GLOBAL_PREFIX + '-tray';

      var header = document.createElement('div');
      header.className = GLOBAL_PREFIX + '-tray-header';
      header.textContent = 'Pending comments (0)';
      container.appendChild(header);

      var list = document.createElement('div');
      list.className = GLOBAL_PREFIX + '-tray-list';
      container.appendChild(list);

      var reason = document.createElement('div');
      reason.className = GLOBAL_PREFIX + '-tray-reason';
      reason.textContent = 'Cannot parse session id from this artifact URL.';
      container.appendChild(reason);

      var actions = document.createElement('div');
      actions.className = GLOBAL_PREFIX + '-tray-actions';
      var clearBtn = document.createElement('button');
      clearBtn.type = 'button';
      clearBtn.textContent = 'Clear';
      clearBtn.className = GLOBAL_PREFIX + '-tray-clear';
      clearBtn.addEventListener('click', clearAll);
      var sendBtn = document.createElement('button');
      sendBtn.type = 'button';
      sendBtn.textContent = 'Send';
      sendBtn.className = GLOBAL_PREFIX + '-tray-send';
      sendBtn.addEventListener('click', sendBatch);
      actions.appendChild(clearBtn);
      actions.appendChild(sendBtn);
      container.appendChild(actions);

      document.body.appendChild(container);
      tray = container;
      trayHeader = header;
      trayList = list;
      traySendBtn = sendBtn;
      trayClearBtn = clearBtn;
      trayReason = reason;

      refreshTray();
    }

    function buildTrayItem(idx, entry) {
      var item = document.createElement('div');
      item.className = GLOBAL_PREFIX + '-tray-item';

      var quote = document.createElement('div');
      quote.className = GLOBAL_PREFIX + '-tray-item-quote';
      quote.textContent = '\u201C' + entry.quote + '\u201D';

      var comment = document.createElement('div');
      comment.className = GLOBAL_PREFIX + '-tray-item-comment';
      comment.textContent = entry.comment;

      var remove = document.createElement('button');
      remove.type = 'button';
      remove.className = GLOBAL_PREFIX + '-tray-remove';
      remove.textContent = '\u00D7';
      remove.title = 'Remove this comment';
      remove.setAttribute('aria-label', 'Remove this comment');
      remove.addEventListener('click', function() {
        removeEntry(idx);
      });

      item.appendChild(remove);
      item.appendChild(quote);
      item.appendChild(comment);
      return item;
    }

    function refreshTray() {
      if (!tray) return;
      trayHeader.textContent = 'Pending comments (' + pending.length + ')';
      trayList.innerHTML = '';
      for (var i = 0; i < pending.length; i++) {
        trayList.appendChild(buildTrayItem(i, pending[i]));
      }
      traySendBtn.textContent = 'Send ' + pending.length;
      tray.style.display = pending.length > 0 ? 'flex' : 'none';
      if (!sessionId) {
        traySendBtn.disabled = true;
        trayReason.style.display = 'block';
      } else {
        traySendBtn.disabled = false;
        trayReason.style.display = 'none';
      }
    }

    function removeEntry(idx) {
      if (idx < 0 || idx >= pending.length) return;
      var removed = pending.splice(idx, 1)[0];
      var stillMarked = false;
      for (var i = 0; i < pending.length; i++) {
        if (pending[i].el === removed.el) { stillMarked = true; break; }
      }
      if (!stillMarked) removed.el.classList.remove(GLOBAL_PREFIX + '-marked');
      refreshTray();
    }

    function clearAll() {
      for (var i = 0; i < pending.length; i++) {
        pending[i].el.classList.remove(GLOBAL_PREFIX + '-marked');
      }
      pending = [];
      refreshTray();
    }

    async function sendBatch() {
      if (!sessionId || pending.length === 0) return;
      var count = pending.length;
      var ordered = pending.slice();
      ordered.sort(function(a, b) {
        if (!a.el || !b.el || typeof a.el.compareDocumentPosition !== 'function' || typeof b.el.compareDocumentPosition !== 'function') return 0;
        try {
          var mask = a.el.compareDocumentPosition(b.el);
          if (mask & Node.DOCUMENT_POSITION_FOLLOWING) return -1;
          if (mask & Node.DOCUMENT_POSITION_PRECEDING) return 1;
          return 0;
        } catch (e) {
          return 0;
        }
      });
      var sendBtn = traySendBtn;
      var prevLabel = sendBtn.textContent;
      sendBtn.disabled = true;
      sendBtn.textContent = 'Sending';
      try {
        await postChatMessage(buildBatchMessage(ordered));
        for (var i = 0; i < pending.length; i++) {
          pending[i].el.classList.remove(GLOBAL_PREFIX + '-marked');
        }
        pending = [];
        refreshTray();
        showToast(count + ' comments sent to chat', false);
      } catch (err) {
        console.error('Batch comment send failed:', err);
        showToast(err.message, true);
        sendBtn.disabled = false;
        sendBtn.textContent = prevLabel;
      }
    }

    function reposition() {
      if (!hovered) return;
      if (popover) positionPopover(popover, hovered);
      else positionTrigger(hovered);
    }
  })();
}
