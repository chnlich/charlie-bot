// ---------------------------------------------------------------------------
// Voice input
// ---------------------------------------------------------------------------
let mediaRecorder = null;
let audioChunks = [];
let isRecording = false;

async function toggleVoice() {
  if (isRecording) {
    stopRecording();
  } else {
    startRecording();
  }
}

async function startRecording() {
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    alert('Microphone access is not available. Please use HTTPS or a supported browser.');
    return;
  }
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });

    // Pick the best supported MIME type for this browser/OS.
    const mimeTypes = [
      'audio/webm;codecs=opus',
      'audio/webm',
      'audio/mp4',
      'audio/ogg;codecs=opus',
      '',
    ];
    const mimeType = mimeTypes.find(t => t === '' || MediaRecorder.isTypeSupported(t)) ?? '';

    mediaRecorder = mimeType
      ? new MediaRecorder(stream, { mimeType })
      : new MediaRecorder(stream);

    // Derive a sensible file extension from the chosen MIME type.
    let ext = '.audio';
    if (mimeType.includes('webm')) ext = '.webm';
    else if (mimeType.includes('mp4')) ext = '.mp4';
    else if (mimeType.includes('ogg')) ext = '.ogg';

    audioChunks = [];
    mediaRecorder.ondataavailable = (e) => { if (e.data.size > 0) audioChunks.push(e.data); };
    mediaRecorder.onstop = async () => {
      stream.getTracks().forEach(t => t.stop());
      const blobType = mimeType || 'audio/webm';
      const blob = new Blob(audioChunks, { type: blobType });
      blob._ext = ext;
      await transcribeAudio(blob);
    };
    mediaRecorder.start(100);
    isRecording = true;
    document.getElementById('voice-btn').classList.add('bg-red-600', 'border-red-500');
    document.getElementById('voice-btn').classList.remove('bg-slate-800', 'border-slate-600');
  } catch (err) {
    console.error('Mic access failed:', err);
    alert('Microphone access failed. Make sure you are using HTTPS and have granted microphone permission.');
  }
}

function stopRecording() {
  if (mediaRecorder && mediaRecorder.state !== 'inactive') {
    mediaRecorder.stop();
  }
  isRecording = false;
  document.getElementById('voice-btn').classList.remove('bg-red-600', 'border-red-500');
  document.getElementById('voice-btn').classList.add('bg-slate-800', 'border-slate-600');
}

async function transcribeAudio(blob) {
  const form = new FormData();
  form.append('audio', blob, 'recording' + (blob._ext || '.webm'));
  form.append('session_id', SESSION_ID);
  pendingUserMsg = true;
  try {
    const res = await fetch('/api/voice/transcribe', { method: 'POST', body: form });
    const data = await res.json();
    if (data.transcription) {
      appendMessage('user', data.transcription, true);
      startThinking();
    }
  } catch (err) {
    console.error('Transcription failed:', err);
  }
}
