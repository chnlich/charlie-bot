import { useCallback, useEffect, useRef, useState } from 'react'
import { Mic, MicOff, Send, X } from 'lucide-react'
import { clsx } from 'clsx'
import { useVoiceRecorder } from '../../hooks/useVoiceRecorder'
import { voiceApi } from '../../api/voice'

interface Props {
  sessionId: string
  onSend: (content: string, isVoice: boolean) => void
  disabled: boolean
}

export function ChatInput({ sessionId, onSend, disabled }: Props) {
  const [text, setText] = useState('')
  const [voiceDisclaimer, setVoiceDisclaimer] = useState<string | null>(null)
  const [transcribing, setTranscribing] = useState(false)
  const [isVoice, setIsVoice] = useState(false)
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const { isRecording, audioBlob, startRecording, stopRecording, clearRecording, error: recError } =
    useVoiceRecorder()

  // Auto-resize textarea
  useEffect(() => {
    const ta = textareaRef.current
    if (!ta) return
    ta.style.height = 'auto'
    ta.style.height = `${Math.min(ta.scrollHeight, 200)}px`
  }, [text])

  // When recording stops and blob is ready, transcribe
  useEffect(() => {
    if (!audioBlob) return
    const transcribe = async () => {
      setTranscribing(true)
      try {
        const result = await voiceApi.transcribe(audioBlob, sessionId)
        setText(result.transcription)
        setVoiceDisclaimer(result.disclaimer)
        setIsVoice(true)
      } catch (e) {
        console.error('Transcription failed', e)
      } finally {
        setTranscribing(false)
        clearRecording()
      }
    }
    transcribe()
  }, [audioBlob, sessionId, clearRecording])

  const handleSend = useCallback(() => {
    const content = text.trim()
    if (!content || disabled) return
    onSend(content, isVoice)
    setText('')
    setVoiceDisclaimer(null)
    setIsVoice(false)
  }, [text, disabled, isVoice, onSend])

  const toggleVoice = async () => {
    if (isRecording) {
      stopRecording()
    } else {
      await startRecording()
    }
  }

  return (
    <div className="border-t border-border px-4 py-3">
      {voiceDisclaimer && (
        <div className="flex items-start gap-2 mb-2 bg-yellow-500/10 border border-yellow-500/20 rounded-lg px-3 py-2 text-xs text-yellow-300">
          <Mic size={12} className="shrink-0 mt-0.5" />
          <span className="flex-1">{voiceDisclaimer}</span>
          <button onClick={() => setVoiceDisclaimer(null)} className="text-yellow-500 hover:text-yellow-300">
            <X size={12} />
          </button>
        </div>
      )}
      {recError && <p className="text-red-400 text-xs mb-2">{recError}</p>}

      <div className="flex items-end gap-2">
        <textarea
          ref={textareaRef}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              handleSend()
            }
          }}
          placeholder="Message CharlieBot... (Enter to send, Shift+Enter for newline)"
          disabled={disabled || transcribing}
          rows={1}
          className="flex-1 resize-none bg-slate-800 border border-border rounded-xl px-4 py-2.5 text-sm text-white placeholder-slate-500 focus:outline-none focus:border-blue-500 disabled:opacity-50"
        />

        {/* Voice button */}
        <button
          onClick={toggleVoice}
          disabled={disabled || transcribing}
          title={isRecording ? 'Stop recording' : 'Start voice input'}
          className={clsx(
            'p-2.5 rounded-xl transition-colors disabled:opacity-50',
            isRecording
              ? 'bg-red-600 text-white animate-pulse'
              : 'bg-slate-700 text-slate-400 hover:text-white hover:bg-slate-600',
          )}
        >
          {isRecording ? <MicOff size={16} /> : <Mic size={16} />}
        </button>

        {/* Send button */}
        <button
          onClick={handleSend}
          disabled={!text.trim() || disabled || transcribing}
          className="p-2.5 bg-blue-600 hover:bg-blue-700 disabled:opacity-30 disabled:cursor-not-allowed text-white rounded-xl transition-colors"
        >
          <Send size={16} />
        </button>
      </div>
    </div>
  )
}
