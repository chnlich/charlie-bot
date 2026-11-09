// ---------------------------------------------------------------------------
// The draft card: one message or publication in the operator's name, waiting
// for the operator's click.
//
// The card shows the target and the whole registered body — no fold, no line
// clamp, no ellipsis — because the server sends exactly that text from the
// operator's account. The one button posts the send route. The three draft
// events are chat messages: the card is the `draft_registered` message, and a
// later `draft_sent`, `draft_failed`, or replacing card updates it in place.
// A sent or superseded card loses its button; a failed card keeps it, and an
// `unknown` failure tells the operator to check the target before resending.
// ---------------------------------------------------------------------------
(function() {
  const Chat = globalThis.Chat;
  const escapeHtml = Chat.escapeHtml;
  const escapeHtmlAttr = Chat.escapeHtmlAttr;
  const escapeJsSingleQuoted = Chat.escapeJsSingleQuoted;

  const CARD_CLASS = 'draft-card max-w-[90%] bg-slate-800 border border-slate-700 rounded-xl px-4 py-3 text-sm';
  const BODY_CLASS = 'draft-body mt-2 text-slate-200 whitespace-pre-wrap break-words';
  const STATE_CLASS = 'draft-state-line mt-2 text-xs text-slate-400';
  const BUTTON_CLASS = 'draft-send mt-2 px-3 py-1.5 rounded bg-slate-600 hover:bg-slate-500 text-white text-xs font-semibold';

  // The card's status names: the `data-draft-status` values, and the state the send path reports.
  const DRAFT_STATUS = {
    SENDABLE: 'sendable',
    SENT: 'sent',
    SUPERSEDED: 'superseded',
    STALE: 'stale',
    FAILED: 'failed',
  };

  // The card's target line: the platform's own label, or the target fields in order.
  function targetText(msg) {
    if (msg.target_label != null && String(msg.target_label) !== '') return String(msg.target_label);
    const target = msg.target && typeof msg.target === 'object' ? msg.target : {};
    return Object.keys(target).map((key) => key + ': ' + String(target[key])).join(' · ');
  }

  function stateLineHtml(state) {
    if (!state || !state.status || state.status === DRAFT_STATUS.SENDABLE) return '';
    if (state.status === DRAFT_STATUS.SENT) {
      const link = state.url
        ? ' · <a class="underline text-blue-400" href="' + escapeHtmlAttr(state.url) + '" target="_blank" rel="noopener">Open</a>'
        : '';
      return 'Sent by ' + escapeHtml(state.sender || '') + link;
    }
    if (state.status === DRAFT_STATUS.SUPERSEDED) return 'Superseded by a newer draft';
    if (state.status === DRAFT_STATUS.STALE) return 'This draft was already sent or replaced';
    if (state.status === DRAFT_STATUS.FAILED) {
      const hint = state.outcome === 'unknown' ? ' Check the target before resending.' : '';
      return 'Send failed: ' + escapeHtml(state.detail || '') + hint;
    }
    return '';
  }

  function sendButtonHtml(sessionId, draftId) {
    return '<button type="button" class="' + BUTTON_CLASS + '"'
      + ' onclick="sendDraft(\'' + escapeJsSingleQuoted(sessionId) + '\', \'' + escapeJsSingleQuoted(draftId) + '\', this)">'
      + 'Send as me</button>';
  }

  // One registered draft's card, as a sendable card.
  function draftCardHtml(msg, sessionId) {
    const draftId = msg.draft_id == null ? '' : String(msg.draft_id);
    const replaces = msg.replaces == null
      ? ''
      : ' data-draft-replaces="' + escapeHtmlAttr(msg.replaces) + '"';
    return '<div class="' + CARD_CLASS + '"' + Chat.messageIdentityAttrs(msg)
      + ' data-draft-id="' + escapeHtmlAttr(draftId) + '"'
      + ' data-draft-status="' + DRAFT_STATUS.SENDABLE + '"' + replaces + '>'
      + '<div class="flex items-center gap-2 text-blue-400 text-xs font-semibold">'
      + '<span>Draft</span><span class="text-slate-400">' + escapeHtml(msg.platform || '') + '</span></div>'
      + '<div class="draft-target mt-1 text-slate-300">To: ' + escapeHtml(targetText(msg)) + '</div>'
      + '<div class="' + BODY_CLASS + '">' + escapeHtml(msg.body == null ? '' : String(msg.body)) + '</div>'
      + '<div class="' + STATE_CLASS + '"></div>'
      + sendButtonHtml(sessionId || '', draftId)
      + '</div>';
  }

  function draftSentLineHtml(msg) {
    return '<div class="draft-outcome w-full bg-slate-700/40 border border-slate-600/30 rounded-lg px-4 py-2 text-xs text-slate-300"'
      + ' data-draft-sent="' + escapeHtmlAttr(msg.draft_id) + '"'
      + ' data-sender="' + escapeHtmlAttr(msg.sender) + '"'
      + ' data-url="' + escapeHtmlAttr(msg.url) + '">'
      + 'Draft sent by ' + escapeHtml(msg.sender == null ? '' : String(msg.sender)) + '</div>';
  }

  function draftFailedLineHtml(msg) {
    return '<div class="draft-outcome w-full bg-red-900/20 border border-red-700/30 rounded-lg px-4 py-2 text-xs text-red-400"'
      + ' data-draft-failed="' + escapeHtmlAttr(msg.draft_id) + '"'
      + ' data-error="' + escapeHtmlAttr(msg.error) + '"'
      + ' data-outcome="' + escapeHtmlAttr(msg.outcome) + '">'
      + 'Draft send failed</div>';
  }

  function queryAll(root, selector) {
    return root && root.querySelectorAll ? Array.from(root.querySelectorAll(selector)) : [];
  }

  function findCard(root, draftId) {
    if (draftId == null || draftId === '') return null;
    const selector = '[data-draft-id="' + String(draftId).replace(/"/g, '\\"') + '"]';
    const local = root && root.querySelector ? root.querySelector(selector) : null;
    if (local) return local;
    return typeof document !== 'undefined' && document.querySelector ? document.querySelector(selector) : null;
  }

  // Apply one state to one card: the status attribute, the state line, and the button's presence.
  function applyDraftState(card, state) {
    if (!card) return;
    const status = state && state.status ? state.status : DRAFT_STATUS.SENDABLE;
    card.dataset.draftStatus = status;
    const line = card.querySelector('.draft-state-line');
    if (line) line.innerHTML = stateLineHtml(state);
    const button = card.querySelector('.draft-send');
    if (button && (status === DRAFT_STATUS.SENT || status === DRAFT_STATUS.SUPERSEDED
      || status === DRAFT_STATUS.STALE)) {
      button.remove();
    }
  }

  // Apply the outcome markers in one rendered root. A marker appended on its own names a card that
  // already sits in the document, so `findCard` falls back to the document.
  function applyDraftUpdates(root) {
    queryAll(root, '[data-draft-replaces]').forEach((marker) => {
      const card = findCard(root, marker.getAttribute('data-draft-replaces'));
      applyDraftState(card, {status: DRAFT_STATUS.SUPERSEDED});
    });
    queryAll(root, '[data-draft-sent]').forEach((marker) => {
      applyDraftState(findCard(root, marker.getAttribute('data-draft-sent')), {
        status: DRAFT_STATUS.SENT,
        sender: marker.getAttribute('data-sender') || '',
        url: marker.getAttribute('data-url') || '',
      });
    });
    queryAll(root, '[data-draft-failed]').forEach((marker) => {
      applyDraftState(findCard(root, marker.getAttribute('data-draft-failed')), {
        status: DRAFT_STATUS.FAILED,
        detail: marker.getAttribute('data-error') || '',
        outcome: marker.getAttribute('data-outcome') || 'rejected',
      });
    });
  }

  // The card button: POST the send route and apply its outcome. The server holds the send lock and
  // owns the outcome event; this only reflects it immediately.
  async function sendDraft(sessionId, draftId, button) {
    if (button) button.disabled = true;
    const card = findCard(null, draftId);
    try {
      const res = await fetch(
          '/api/sessions/' + encodeURIComponent(sessionId) + '/drafts/' + encodeURIComponent(draftId) + '/send',
          {method: 'POST', headers: JSON_HEADERS});
      const body = await res.json().catch(() => ({}));
      if (res.status === 409) {
        applyDraftState(card, {status: DRAFT_STATUS.STALE});
        return;
      }
      if (!res.ok) {
        applyDraftState(card, {
          status: DRAFT_STATUS.FAILED,
          detail: body.detail || ('HTTP ' + res.status),
          outcome: body.outcome || 'rejected',
        });
        return;
      }
      applyDraftState(card, {status: DRAFT_STATUS.SENT, sender: body.sender || '', url: body.url || ''});
    } catch (error) {
      applyDraftState(card, {status: DRAFT_STATUS.FAILED, detail: String(error), outcome: 'unknown'});
    } finally {
      // A failed send leaves the button clickable: the operator decides whether to resend, and the
      // server never retries on its own. A sent or superseded card lost its button above.
      if (button) button.disabled = false;
    }
  }

  Chat.registerMessageRoleRenderer('draft', (msg, sessionId) => draftCardHtml(msg, sessionId));
  Chat.registerMessageRoleRenderer('draft_sent', draftSentLineHtml);
  Chat.registerMessageRoleRenderer('draft_failed', draftFailedLineHtml);
  Chat.registerMessagePostProcessor(applyDraftUpdates);
  Chat.wire(
      {sendDraft},
      {draftCardHtml, draftSentLineHtml, draftFailedLineHtml, applyDraftState, applyDraftUpdates, stateLineHtml});
})();
