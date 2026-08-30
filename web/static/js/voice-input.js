// ---------------------------------------------------------------------------
// Voice input
// ---------------------------------------------------------------------------
let voiceSocket = null;
let voiceStream = null;
let voiceAudioContext = null;
let voiceSourceNode = null;
let voiceWorkletNode = null;
let isRecording = false;
let voiceStopping = false;
let voiceAwaitingFinal = false;
let voiceFlushId = 0;
let voiceFlushResolvers = new Map();

const VOICE_CHUNK_SAMPLES = 2048;

const VOICE_WORKLET_SOURCE = `
class VoiceCaptureProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.inputRate = options.processorOptions.inputSampleRate;
    this.outputRate = 16000;
    this.ratio = this.inputRate / this.outputRate;
    this.sourceRemainder = new Float32Array(0);
    this.sourcePosition = 0;
    this.outputSamples = [];
    this.chunkSamples = options.processorOptions.chunkSamples;
    this.port.onmessage = (event) => {
      if (event.data && event.data.type === 'flush') {
        this.flush(event.data.id);
      }
    };
  }

  process(inputs) {
    const input = inputs[0];
    if (!input || !input[0] || input[0].length === 0) return true;
    this.consume(input[0]);
    return true;
  }

  consume(channel) {
    const merged = new Float32Array(this.sourceRemainder.length + channel.length);
    merged.set(this.sourceRemainder, 0);
    merged.set(channel, this.sourceRemainder.length);

    while (this.sourcePosition + 1 < merged.length) {
      const index = Math.floor(this.sourcePosition);
      const fraction = this.sourcePosition - index;
      const sample = merged[index] + (merged[index + 1] - merged[index]) * fraction;
      this.outputSamples.push(this.toInt16(sample));
      if (this.outputSamples.length >= this.chunkSamples) {
        this.sendChunk(this.chunkSamples);
      }
      this.sourcePosition += this.ratio;
    }

    const consumed = Math.floor(this.sourcePosition);
    this.sourceRemainder = merged.slice(consumed);
    this.sourcePosition -= consumed;
  }

  toInt16(sample) {
    const clipped = Math.max(-1, Math.min(1, sample));
    return clipped < 0 ? Math.round(clipped * 32768) : Math.round(clipped * 32767);
  }

  sendChunk(count) {
    const values = this.outputSamples.splice(0, count);
    const pcm = new Int16Array(values.length);
    for (let i = 0; i < values.length; i++) pcm[i] = values[i];
    this.port.postMessage({type: 'pcm', buffer: pcm.buffer}, [pcm.buffer]);
  }

  flush(id) {
    if (this.outputSamples.length > 0) {
      this.sendChunk(this.outputSamples.length);
    }
    this.port.postMessage({type: 'flushed', id});
  }
}

registerProcessor('voice-capture', VoiceCaptureProcessor);
`;

async function toggleVoice() {
  if (isRecording) {
    await stopRecording();
  } else {
    await startRecording();
  }
}

async function startRecording() {
  if (!SESSION_ID) {
    showToast('Open a session before recording voice input', true);
    return;
  }
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    showToast('Microphone access is not available. Use HTTPS or a supported browser.', true);
    return;
  }
  if (!window.AudioContext && !window.webkitAudioContext) {
    showToast('Audio capture is not available in this browser', true);
    return;
  }

  const targetSession = SESSION_ID;
  try {
    voiceStream = await navigator.mediaDevices.getUserMedia({
      audio: {
        channelCount: 1,
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
    });

    voiceSocket = await openVoiceSocket(targetSession);
    setupVoiceSocketHandlers(voiceSocket, targetSession);

    const AudioContextCtor = window.AudioContext || window.webkitAudioContext;
    voiceAudioContext = new AudioContextCtor();
    const workletUrl = URL.createObjectURL(new Blob([VOICE_WORKLET_SOURCE], {type: 'application/javascript'}));
    try {
      await voiceAudioContext.audioWorklet.addModule(workletUrl);
    } finally {
      URL.revokeObjectURL(workletUrl);
    }

    voiceSourceNode = voiceAudioContext.createMediaStreamSource(voiceStream);
    voiceWorkletNode = new AudioWorkletNode(voiceAudioContext, 'voice-capture', {
      numberOfInputs: 1,
      numberOfOutputs: 0,
      processorOptions: {
        inputSampleRate: voiceAudioContext.sampleRate,
        chunkSamples: VOICE_CHUNK_SAMPLES,
      },
    });
    voiceWorkletNode.port.onmessage = handleVoiceWorkletMessage;
    voiceSourceNode.connect(voiceWorkletNode);

    isRecording = true;
    voiceStopping = false;
    voiceAwaitingFinal = false;
    setVoiceButtonRecording(true);
    showVoiceOverlay('Listening...');
  } catch (err) {
    console.error('Voice input failed:', err);
    showToast('Voice input failed: ' + err.message, true);
    cleanupVoiceCapture();
    removeVoiceOverlay();
  }
}

async function stopRecording() {
  if (!isRecording || voiceStopping) return;
  voiceStopping = true;
  setVoiceButtonRecording(false);
  try {
    await flushVoiceWorklet();
    cleanupVoiceCapture({keepSocket: true});
    if (voiceSocket && voiceSocket.readyState === WebSocket.OPEN) {
      voiceAwaitingFinal = true;
      voiceSocket.send(JSON.stringify({type: 'stop'}));
      showVoiceOverlay('Finalizing...');
    } else {
      discardVoiceRecording('Voice connection closed before transcription finished');
    }
  } catch (err) {
    console.error('Voice stop failed:', err);
    discardVoiceRecording('Voice input failed: ' + err.message);
  }
}

function resetVoiceState() {
  cleanupVoiceCapture();
  closeVoiceSocket();
  removeVoiceOverlay();
  Chat.setVoiceContributed(false);
}

function openVoiceSocket(targetSession) {
  return new Promise((resolve, reject) => {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let wsUrl = withAccessToken(`${proto}//${location.host}/ws/voice/${encodeURIComponent(targetSession)}`);
    const socket = new WebSocket(wsUrl);
    socket.binaryType = 'arraybuffer';
    socket.onopen = () => resolve(socket);
    socket.onerror = () => reject(new Error('voice WebSocket connection failed'));
    socket.onclose = () => reject(new Error('voice WebSocket closed before recording started'));
  });
}

function setupVoiceSocketHandlers(socket, targetSession) {
  socket.onmessage = (event) => {
    if (socket !== voiceSocket || targetSession !== SESSION_ID) return;
    let data;
    try {
      data = JSON.parse(event.data);
    } catch (err) {
      console.error('Invalid voice message:', err);
      discardVoiceRecording('Invalid voice response from server');
      return;
    }

    if (data.type === 'partial') {
      showVoiceOverlay(data.text || 'Listening...');
      return;
    }
    if (data.type === 'final') {
      applyVoiceFinal(data.text || '');
      closeVoiceSocket();
      return;
    }
    if (data.type === 'error') {
      discardVoiceRecording(data.text || 'Voice transcription failed');
      return;
    }
    discardVoiceRecording('Invalid voice response from server');
  };

  socket.onclose = () => {
    if (socket !== voiceSocket) return;
    if (isRecording || voiceStopping || voiceAwaitingFinal) {
      discardVoiceRecording('Voice connection closed before transcription finished');
    }
  };

  socket.onerror = () => {
    if (socket !== voiceSocket) return;
    discardVoiceRecording('Voice connection error');
  };
}

function handleVoiceWorkletMessage(event) {
  const data = event.data || {};
  if (data.type === 'pcm') {
    if (voiceSocket && voiceSocket.readyState === WebSocket.OPEN && !voiceStopping) {
      voiceSocket.send(data.buffer);
    }
    return;
  }
  if (data.type === 'flushed') {
    const resolve = voiceFlushResolvers.get(data.id);
    if (resolve) {
      voiceFlushResolvers.delete(data.id);
      resolve();
    }
  }
}

function flushVoiceWorklet() {
  if (!voiceWorkletNode) return Promise.resolve();
  const id = ++voiceFlushId;
  return new Promise((resolve) => {
    voiceFlushResolvers.set(id, resolve);
    voiceWorkletNode.port.postMessage({type: 'flush', id});
    setTimeout(() => {
      const pending = voiceFlushResolvers.get(id);
      if (pending) {
        voiceFlushResolvers.delete(id);
        pending();
      }
    }, 500);
  });
}

function applyVoiceFinal(text) {
  const finalText = text.trim();
  voiceAwaitingFinal = false;
  cleanupVoiceCapture();
  removeVoiceOverlay();
  if (!finalText) {
    showToast('No speech detected');
    return;
  }

  const input = document.getElementById('msg-input');
  const current = input.value.trim();
  input.value = current ? current + ' ' + finalText : finalText;
  Chat.setVoiceContributed(true);
  autoResize(input);
  saveDraft();
  input.focus();
}

function discardVoiceRecording(message) {
  cleanupVoiceCapture();
  voiceAwaitingFinal = false;
  closeVoiceSocket();
  removeVoiceOverlay();
  if (message) showToast(message, true);
}

function cleanupVoiceCapture(options) {
  const keepSocket = options && options.keepSocket;
  isRecording = false;
  voiceStopping = false;
  setVoiceButtonRecording(false);

  if (voiceSourceNode) {
    try { voiceSourceNode.disconnect(); } catch (err) { console.warn('Voice source disconnect failed:', err); }
    voiceSourceNode = null;
  }
  if (voiceWorkletNode) {
    voiceWorkletNode.port.onmessage = null;
    try { voiceWorkletNode.disconnect(); } catch (err) { console.warn('Voice worklet disconnect failed:', err); }
    voiceWorkletNode = null;
  }
  if (voiceAudioContext) {
    const ctx = voiceAudioContext;
    voiceAudioContext = null;
    ctx.close().catch((err) => console.warn('Voice audio context close failed:', err));
  }
  if (voiceStream) {
    voiceStream.getTracks().forEach((track) => track.stop());
    voiceStream = null;
  }
  voiceFlushResolvers.forEach((resolve) => resolve());
  voiceFlushResolvers.clear();
  if (!keepSocket) closeVoiceSocket();
}

function closeVoiceSocket() {
  const socket = voiceSocket;
  voiceSocket = null;
  if (!socket) return;
  socket.onopen = null;
  socket.onmessage = null;
  socket.onclose = null;
  socket.onerror = null;
  if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) {
    socket.close();
  }
}

function setVoiceButtonRecording(recording) {
  const btn = document.getElementById('voice-btn');
  if (!btn) return;
  if (recording) {
    btn.classList.add('bg-red-600', 'border-red-500');
    btn.classList.remove('bg-slate-800', 'border-slate-600');
  } else {
    btn.classList.remove('bg-red-600', 'border-red-500');
    btn.classList.add('bg-slate-800', 'border-slate-600');
  }
}

function showVoiceOverlay(text) {
  const input = document.getElementById('msg-input');
  if (!input || !input.parentElement) return;
  let overlay = document.getElementById('voice-partial-overlay');
  if (!overlay) {
    overlay = document.createElement('div');
    overlay.id = 'voice-partial-overlay';
    overlay.className = 'absolute left-0 right-0 bottom-full mb-2 rounded-lg border border-blue-500/40 bg-slate-800 px-3 py-2 text-sm text-slate-100 shadow-lg max-h-28 overflow-y-auto';
    input.parentElement.appendChild(overlay);
  }
  overlay.textContent = text || 'Listening...';
}

function removeVoiceOverlay() {
  document.getElementById('voice-partial-overlay')?.remove();
}
