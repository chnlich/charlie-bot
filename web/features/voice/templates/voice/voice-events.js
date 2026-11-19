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

let voiceInputBusy = false;
let voiceInputStartPending = null;

// The first-click state is visible before the lazy module arrives.
function setVoiceButtonState(state) {
  const button = document.getElementById('voice-btn');
  const styles = {
    starting: ['bg-amber-600', 'border-amber-500'],
    recording: ['bg-red-600', 'border-red-500'],
    idle: ['bg-slate-800', 'border-slate-600'],
  };
  const classes = styles[state];
  if (!classes) throw new Error('Unknown voice button state: ' + state);
  button.classList.remove('bg-slate-800', 'border-slate-600', 'bg-amber-600', 'border-amber-500',
      'bg-red-600', 'border-red-500');
  button.classList.add(...classes);
  button.dataset.state = state;
  if (state === 'starting') {
    button.title = 'Preparing microphone…';
    button.setAttribute('aria-label', 'Preparing microphone…');
  } else if (state === 'recording') {
    button.title = 'Speak now';
    button.setAttribute('aria-label', 'Speak now');
  } else {
    button.title = 'Voice input';
    button.setAttribute('aria-label', 'Voice input');
  }
}

// One promise per key: a second caller joins a pending request. The stylesheet
// and module start together. Neither delays the other.
function loadVoiceInput() {
  return Promise.all([
    loadPageStyles('voice', ['/static/features/voice/voice-overlay.css']),
    loadPageScripts('voice', ['/static/features/voice/voice-input.js']),
  ]);
}

function openVoiceInput() {
  if (voiceInputStartPending) return;
  if (voiceInputBusy) {
    loadVoiceInput().then(() => toggleVoice()).catch(handleVoiceInputLoadError);
    return;
  }
  if (!SESSION_ID) {
    showToast('Open a session before recording voice input', true);
    return;
  }
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    showToast('Microphone access is not available. Use HTTPS or a supported browser.', true);
    return;
  }
  const AudioContextCtor = window.AudioContext || window.webkitAudioContext;
  if (!AudioContextCtor) {
    showToast('Audio capture is not available in this browser', true);
    return;
  }

  const request = {
    clickedAt: performance.now(),
    sessionId: SESSION_ID,
    backend: selectedVoiceBackend(),
    micOpenMs: null,
    micPermission: null,
    stream: null,
    micResultPromise: null,
    audioContext: null,
    moduleReady: false,
    contextReady: false,
    error: null,
    runStarted: false,
    cancelled: false,
  };
  voiceInputStartPending = request;
  setVoiceButtonState('starting');

  const fail = (error) => {
    if (voiceInputStartPending !== request) return;
    voiceInputStartPending = null;
    request.cancelled = true;
    request.stream?.getTracks().forEach((track) => track.stop());
    request.audioContext?.close().catch((closeError) => console.error('Voice audio engine close failed:', closeError));
    setVoiceButtonState('idle');
    updateVoiceMicTitle();
    const message = error && error.message ? error.message : String(error);
    showToast('Voice input failed: ' + message, true);
  };
  const startWhenReady = () => {
    if (request.cancelled || voiceInputStartPending !== request) return;
    if (request.error) {
      fail(request.error);
      return;
    }
    if (!request.moduleReady || !request.contextReady) return;
    request.runStarted = true;
    voiceInputStartPending = null;
    toggleVoice(request).catch((error) => {
      voiceInputBusy = false;
      setVoiceButtonState('idle');
      updateVoiceMicTitle();
      handleVoiceInputLoadError(error);
    });
  };

  try {
    request.audioContext = new AudioContextCtor();
  } catch (error) {
    request.error = error;
    fail(error);
    return;
  }
  try {
    const resume = request.audioContext.resume();
    request.contextReady = true;
    startWhenReady();
    Promise.resolve(resume).catch((error) => console.warn('Voice audio engine resume failed:', error));
  } catch (error) {
    request.contextReady = true;
    request.resumeError = error;
    console.warn('Voice audio engine resume failed:', error);
    startWhenReady();
  }
  let micPromise;
  try {
    micPromise = navigator.mediaDevices.getUserMedia({
      audio: {channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true},
    });
  } catch (error) {
    micPromise = Promise.reject(error);
  }
  request.micResultPromise = Promise.resolve(micPromise).then((stream) => {
    if (request.cancelled || (!request.runStarted && voiceInputStartPending !== request)) {
      stream.getTracks().forEach((track) => track.stop());
      return {cancelled: true};
    }
    request.stream = stream;
    request.micOpenMs = Math.max(0, Math.round(performance.now() - request.clickedAt));
    return {stream};
  }, (error) => ({error}));
  if (navigator.permissions && typeof navigator.permissions.query === 'function') {
    navigator.permissions.query({name: 'microphone'}).then((status) => {
      request.micPermission = status.state;
      if (activeVoiceRun && activeVoiceRun.startup.clickedAt === request.clickedAt) {
        activeVoiceRun.startup.micPermission = status.state;
      }
      if (status.state === 'prompt' && (voiceInputStartPending === request ||
          (request.runStarted && activeVoiceRun && activeVoiceRun.startRequest === request))) {
        const button = document.getElementById('voice-btn');
        if (button && button.dataset.state === 'starting') {
          button.title = 'Allow the microphone in Chrome’s prompt';
          button.setAttribute('aria-label', 'Allow the microphone in Chrome’s prompt');
        }
      }
    }).catch((error) => console.warn('Voice microphone permission query failed:', error));
  }
  loadVoiceInput().then(() => {
    request.moduleReady = true;
    startWhenReady();
  }, fail);
}

function handleVoiceInputLoadError(error) {
  console.error('Voice input failed:', error);
  showToast('Voice input failed: ' + error.message, true);
}

function cancelPendingVoiceInput() {
  const request = voiceInputStartPending;
  if (!request) return;
  voiceInputStartPending = null;
  request.cancelled = true;
  request.stream?.getTracks().forEach((track) => track.stop());
  request.audioContext?.close().catch((error) => console.error('Voice audio engine close failed:', error));
}

function toggleVoiceBackend() {
  loadVoiceInput().then(() => toggleVoiceBackendMenu()).catch(handleVoiceInputLoadError);
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
    loadVoiceInput().then(() => openVoiceBackendMenu()).catch(handleVoiceInputLoadError);
  };
  overflow.appendChild(entry);
}

// A dictated message's bubble carries the microphone badge from the first
// paint, before any recording loads the module.
Chat.registerInputMode(VOICE_INPUT_MODE, cbIcon('voice', 12) + ' Voice');
updateVoiceMicTitle();
ensureVoiceBackendEntry();

window.addEventListener('pagehide', cancelPendingVoiceInput);
document.addEventListener('charliebot:session-view-teardown', cancelPendingVoiceInput);
window.addEventListener('load', () => {
  if (typeof window.requestIdleCallback === 'function') {
    window.requestIdleCallback(() => {
      loadVoiceInput().catch(handleVoiceInputLoadError);
    });
  }
}, {once: true});
