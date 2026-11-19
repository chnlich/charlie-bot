
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

// The two ratings the server stores and the colors each one takes. Every site reads its rating's entry, so an
// unknown rating fails here instead of wearing the other rating's color.
const RATING_STYLES = new Map([
  ['thumbs_up', { active: 'text-green-400', hover: 'hover:text-green-400' }],
  ['thumbs_down', { active: 'text-red-400', hover: 'hover:text-red-400' }],
]);

function ratingStyle(rating) {
  const style = RATING_STYLES.get(rating);
  if (!style) throw new Error('unknown round rating: ' + rating);
  return style;
}

function roundRatingButtonClass(buttonRating, activeRating) {
  const style = ratingStyle(buttonRating);
  const active = buttonRating === activeRating;
  return 'round-rating-button p-0.5 text-sm leading-none transition-colors duration-150 ' + (active ? style.active : 'text-slate-500 ' + style.hover);
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
    + cbIcon('rate-up', 14) + '</button>'
    + '<button type="button" data-round-rating="thumbs_down"' + sharedAttrs
    + ' aria-pressed="' + String(activeRating === 'thumbs_down') + '"'
    + ' onclick="rateRound(\'' + sessionArg + '\', \'' + roundArg + '\', \'thumbs_down\')"'
    + ' class="' + roundRatingButtonClass('thumbs_down', activeRating) + '" title="Thumbs down">'
    + cbIcon('rate-down', 14) + '</button>';
}

function applyRoundRatingButtonState(btn, activeRating) {
  const buttonRating = btn.dataset.roundRating;
  const style = ratingStyle(buttonRating);
  const isActive = buttonRating === activeRating;
  btn.classList.toggle('text-slate-500', !isActive);
  btn.classList.toggle(style.active, isActive);
  btn.classList.toggle(style.hover, !isActive);
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
    const res = await fetch('/api/sessions/' + sessionId, {signal: viewRequestSignal()});
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
