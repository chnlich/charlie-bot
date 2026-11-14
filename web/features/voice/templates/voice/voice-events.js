// Voice input's page-level registration.
//
// The recording module is a static file, and the first use of the mic, the
// caret or the composer's ⋯ entry loads it (loadVoiceInput below). What a page
// load must already show stays here. That covers the badge of a dictated
// message, the mic's backend tooltip, and the ⋯ menu entry phones use to pick
// a backend.
const VOICE_INPUT_MODE = 'voice';
// Where the caret's choice lives. The server default backs an unknown or
// unavailable stored id.
const VOICE_BACKEND_STORAGE_KEY = 'charliebot-voice-backend';
// On phones the caret is hidden and the composer's ⋯ menu carries this entry
// instead. The entry opens the same menu the caret does.
const VOICE_OVERFLOW_MENU_ID = 'overflow-menu';
const VOICE_OVERFLOW_ENTRY_ID = 'voice-backend-menu-entry';

// The choices ride the page as VOICE_BACKENDS (id, label, livePartials,
// unavailableReason) with the server's VOICE_DEFAULT_BACKEND. The user's pick
// lives in localStorage.
function selectedVoiceBackend() {
  // The stored choice wins while it exists and is configured. An unknown or
  // unavailable stored id falls back to the server default (a typo there fails
  // config load). The stored id stays put, so a credential that arrives later
  // re-activates it on the next page load.
  const stored = localStorage.getItem(VOICE_BACKEND_STORAGE_KEY);
  const storedEntry = stored ? VOICE_BACKENDS.find((entry) => entry.id === stored) : null;
  if (storedEntry && !storedEntry.unavailableReason) return storedEntry;
  return VOICE_BACKENDS.find((entry) => entry.id === VOICE_DEFAULT_BACKEND);
}

function updateVoiceMicTitle() {
  const btn = document.getElementById('voice-btn');
  const selected = selectedVoiceBackend();
  if (btn && selected) btn.title = 'Voice input · ' + selected.label;
}

// One promise per key: a second click while the module travels joins the first
// load. The recording module registers the caret's menu and the run handlers.
// The overlay's rules come first, so the overlay is styled when it appears. A
// stylesheet that fails still lets the module load.
function loadVoiceInput() {
  return loadPageStyles('voice', ['/static/features/voice/voice-overlay.css'])
    .catch((error) => console.error(error.message))
    .then(() => loadPageScripts('voice', ['/static/features/voice/voice-input.js']));
}

function openVoiceInput() {
  loadVoiceInput().then(() => toggleVoice()).catch((error) => console.error(error));
}

function toggleVoiceBackend() {
  loadVoiceInput().then(() => toggleVoiceBackendMenu()).catch((error) => console.error(error));
}

// The composer's ⋯ menu entry, created by this package so the core template
// names nothing it defines. Opening it closes the ⋯ menu first. It drives the
// one backend menu, which belongs to the recording module.
function ensureVoiceBackendEntry() {
  const overflow = document.getElementById(VOICE_OVERFLOW_MENU_ID);
  if (!overflow || document.getElementById(VOICE_OVERFLOW_ENTRY_ID)) return;
  const entry = document.createElement('button');
  entry.type = 'button';
  entry.id = VOICE_OVERFLOW_ENTRY_ID;
  entry.textContent = 'Voice backend';
  entry.onclick = () => {
    overflow.classList.remove('show');
    loadVoiceInput().then(() => openVoiceBackendMenu()).catch((error) => console.error(error));
  };
  overflow.appendChild(entry);
}

// A dictated message's bubble carries the microphone badge from the first
// paint, before any recording loads the module.
Chat.registerInputMode(VOICE_INPUT_MODE, '&#127908; Voice');
updateVoiceMicTitle();
ensureVoiceBackendEntry();
