
(function() {
  const Chat = globalThis.Chat;
  const escapeHtmlAttr = Chat.escapeHtmlAttr;
  const escapeJsSingleQuoted = Chat.escapeJsSingleQuoted;

let activeRoundRatings = {};

function setActiveRoundRatings(roundRatings) {
  activeRoundRatings = roundRatings || {};
}

function getRoundRating(roundId) {
  return activeRoundRatings[String(roundId)] || null;
}

function roundRatingButtonClass(buttonRating, activeRating) {
  const active = buttonRating === activeRating;
  const activeClass = buttonRating === 'thumbs_up' ? 'text-green-400' : 'text-red-400';
  const hoverClass = buttonRating === 'thumbs_up' ? 'hover:text-green-400' : 'hover:text-red-400';
  return 'round-rating-button p-0.5 text-sm leading-none transition-colors duration-150 ' + (active ? activeClass : 'text-slate-500 ' + hoverClass);
}

function renderRoundRatingButtons(sessionId, roundId) {
  const activeRating = getRoundRating(roundId);
  const sessionArg = escapeJsSingleQuoted(sessionId);
  const roundKey = String(roundId);
  const roundArg = escapeJsSingleQuoted(roundKey);
  const sharedAttrs = ' data-round-rating-session="' + escapeHtmlAttr(sessionId) + '"'
    + ' data-round-rating-event="' + escapeHtmlAttr(roundKey) + '"';
  return '<button type="button" data-round-rating="thumbs_up"' + sharedAttrs
    + ' aria-pressed="' + String(activeRating === 'thumbs_up') + '"'
    + ' onclick="rateRound(\'' + sessionArg + '\', \'' + roundArg + '\', \'thumbs_up\')"'
    + ' class="' + roundRatingButtonClass('thumbs_up', activeRating) + '" title="Thumbs up">'
    + '<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6.633 10.5c.806 0 1.533-.446 2.031-1.08a9.041 9.041 0 0 1 2.861-2.4c.723-.384 1.35-.956 1.653-1.715a4.498 4.498 0 0 0 .322-1.672V2.75A.75.75 0 0 1 14.25 2a2.25 2.25 0 0 1 2.25 2.25c0 1.152-.26 2.243-.723 3.218-.266.558.107 1.282.725 1.282h3.126c1.026 0 1.945.694 2.054 1.715.045.422.068.85.068 1.285a11.95 11.95 0 0 1-2.649 7.521c-.388.482-.987.729-1.605.729H13.48c-.483 0-.964-.078-1.423-.23l-3.114-1.04a4.501 4.501 0 0 0-1.423-.23H5.904M14.25 9h2.25M5.904 18.75c.083.205.173.405.27.602.197.4-.078.898-.523.898h-.908c-.889 0-1.713-.518-1.972-1.368a12 12 0 0 1-.521-3.507c0-1.553.295-3.036.831-4.398C3.387 10.203 4.167 9.75 5 9.75h1.053c.472 0 .745.556.5.96a8.958 8.958 0 0 0-1.302 4.665c0 1.194.232 2.333.654 3.375Z"/></svg></button>'
    + '<button type="button" data-round-rating="thumbs_down"' + sharedAttrs
    + ' aria-pressed="' + String(activeRating === 'thumbs_down') + '"'
    + ' onclick="rateRound(\'' + sessionArg + '\', \'' + roundArg + '\', \'thumbs_down\')"'
    + ' class="' + roundRatingButtonClass('thumbs_down', activeRating) + '" title="Thumbs down">'
    + '<svg class="w-3.5 h-3.5" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M7.5 15h2.25m8.024-9.75c.011.05.028.1.052.148.591 1.2.924 2.55.924 3.977a8.96 8.96 0 0 1-.999 4.125m.023-8.25c-.076-.365.183-.75.575-.75h.908c.889 0 1.713.518 1.972 1.368.339 1.11.521 2.287.521 3.507 0 1.553-.295 3.036-.831 4.398C20.613 14.547 19.833 15 19 15h-1.053c-.472 0-.745-.556-.5-.96a8.95 8.95 0 0 0 .303-.54m.023-8.25H16.48a4.5 4.5 0 0 1-1.423-.23l-3.114-1.04a4.5 4.5 0 0 0-1.423-.23H6.504c-.618 0-1.217.247-1.605.729A11.95 11.95 0 0 0 2.25 12c0 .434.023.863.068 1.285C2.427 14.306 3.346 15 4.372 15h3.126c.618 0 .991.724.725 1.282A7.471 7.471 0 0 0 7.5 19.5a2.25 2.25 0 0 0 2.25 2.25.75.75 0 0 0 .75-.75v-.633c0-.573.11-1.14.322-1.672.304-.76.93-1.33 1.653-1.715a9.04 9.04 0 0 0 2.86-2.4c.498-.634 1.226-1.08 2.032-1.08h.384"/></svg></button>';
}

function applyRoundRatingButtonState(btn, activeRating) {
  const buttonRating = btn.dataset.roundRating;
  const isActive = buttonRating === activeRating;
  btn.classList.toggle('text-slate-500', !isActive);
  btn.classList.toggle('text-green-400', buttonRating === 'thumbs_up' && isActive);
  btn.classList.toggle('text-red-400', buttonRating === 'thumbs_down' && isActive);
  btn.classList.toggle('hover:text-green-400', buttonRating === 'thumbs_up' && !isActive);
  btn.classList.toggle('hover:text-red-400', buttonRating === 'thumbs_down' && !isActive);
  btn.setAttribute('aria-pressed', String(isActive));
}

function updateRoundRatingButtons(sessionId, roundId, activeRating) {
  const roundKey = String(roundId);
  document.querySelectorAll('[data-round-rating]').forEach(btn => {
    if (btn.dataset.roundRatingSession === String(sessionId) && btn.dataset.roundRatingEvent === roundKey) {
      applyRoundRatingButtonState(btn, activeRating);
    }
  });
}

function refreshAllRoundRatingButtons(root) {
  root.querySelectorAll('[data-round-rating]').forEach(btn => {
    applyRoundRatingButtonState(btn, getRoundRating(btn.dataset.roundRatingEvent));
  });
}

async function initializeRoundRatings() {
  if (!SESSION_ID) return;
  const sessionId = SESSION_ID;
  try {
    const res = await fetch('/api/sessions/' + sessionId);
    if (!res.ok) throw new Error(`Load round ratings failed: ${res.status}`);
    const session = await res.json();
    if (SESSION_ID !== sessionId) return;
    setActiveRoundRatings(session.round_ratings || {});
    refreshAllRoundRatingButtons(document);
  } catch (err) {
    console.error('Load round ratings failed:', err);
  }
}

async function rateRound(sessionId, roundId, rating) {
  const roundKey = String(roundId);
  const nextRating = activeRoundRatings[roundKey] === rating ? null : rating;
  try {
    const res = await fetch('/api/sessions/' + sessionId + '/rounds/' + encodeURIComponent(roundKey) + '/rate', {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({rating: nextRating}),
    });
    if (!res.ok) throw new Error(`Rate round failed: ${res.status}`);
    const session = await res.json();
    if (SESSION_ID !== sessionId) return;
    setActiveRoundRatings(session.round_ratings || {});
    updateRoundRatingButtons(sessionId, roundKey, getRoundRating(roundKey));
  } catch (err) {
    console.error('Rate round failed:', err);
  }
}

const GLOBALS = {
  setActiveRoundRatings,
  rateRound,
};
const CHAT_ONLY = {
  renderRoundRatingButtons,
  refreshAllRoundRatingButtons,
  initializeRoundRatings,
};
Chat.wire(GLOBALS, CHAT_ONLY);

})();
