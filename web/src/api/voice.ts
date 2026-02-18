import type { VoiceTranscriptionResponse } from '../types'

export const voiceApi = {
  transcribe: async (audioBlob: Blob, sessionId: string): Promise<VoiceTranscriptionResponse> => {
    const form = new FormData()
    form.append('audio', audioBlob, 'recording.webm')
    form.append('session_id', sessionId)

    const res = await fetch('/api/voice/transcribe', {
      method: 'POST',
      body: form,
    })
    if (!res.ok) throw new Error(`Transcription failed: ${res.status}`)
    return res.json()
  },
}
