import { useCallback, useEffect, useState } from 'react'
import { useSSE } from '../../hooks/useSSE'
import { useChatStore } from '../../store/chat'
import type { ChatMessage, SSEEvent } from '../../types'
import { ChatInput } from './ChatInput'
import { MessageList } from './MessageList'

interface Props {
  sessionId: string
}

export function ChatPanel({ sessionId }: Props) {
  const [isStreaming, setIsStreaming] = useState(false)
  const { stream } = useSSE()
  const { messagesBySession, streamingContent, addMessage, setMessages, appendStream, clearStream } =
    useChatStore()

  const messages = messagesBySession[sessionId] ?? []
  const currentStream = streamingContent[sessionId] ?? ''

  // Load history on mount
  useEffect(() => {
    fetch(`/api/chat/${sessionId}/history`)
      .then((r) => r.json())
      .then((h) => setMessages(sessionId, h.messages ?? []))
      .catch(console.error)
  }, [sessionId, setMessages])

  // Subscribe to session WebSocket for worker completion summaries
  useEffect(() => {
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const ws = new WebSocket(`${proto}//${window.location.host}/ws/sessions/${sessionId}`)

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data)
        if (data.type === 'worker_summary') {
          addMessage(sessionId, {
            id: crypto.randomUUID(),
            role: 'assistant',
            content: data.content,
            timestamp: new Date().toISOString(),
            is_voice: false,
            thread_id: data.thread_id ?? null,
          })
        }
      } catch {
        // ignore parse errors
      }
    }

    return () => ws.close()
  }, [sessionId, addMessage])

  const handleSend = useCallback(
    async (content: string, isVoice: boolean) => {
      // Optimistic user message
      const userMsg: ChatMessage = {
        id: crypto.randomUUID(),
        role: 'user',
        content,
        timestamp: new Date().toISOString(),
        is_voice: isVoice,
        thread_id: null,
      }
      addMessage(sessionId, userMsg)
      setIsStreaming(true)

      try {
        await stream(
          `/api/chat/${sessionId}/message`,
          { content },
          (event: unknown) => {
            const e = event as SSEEvent
            if (e.type === 'chunk') {
              appendStream(sessionId, (e as { type: 'chunk'; content: string }).content)
            } else if (e.type === 'task_delegated') {
              const ev = e as { type: string; task_id: string; priority: string; description: string }
              const systemMsg: ChatMessage = {
                id: crypto.randomUUID(),
                role: 'system',
                content: `Task queued [${ev.priority}]: ${ev.description}`,
                timestamp: new Date().toISOString(),
                is_voice: false,
                thread_id: ev.task_id,
              }
              const assistantContent = clearStream(sessionId)
              if (assistantContent.trim()) {
                addMessage(sessionId, {
                  id: crypto.randomUUID(),
                  role: 'assistant',
                  content: assistantContent,
                  timestamp: new Date().toISOString(),
                  is_voice: false,
                  thread_id: null,
                })
              }
              addMessage(sessionId, systemMsg)
            } else if (e.type === 'done') {
              const assistantContent = clearStream(sessionId)
              if (assistantContent.trim()) {
                addMessage(sessionId, {
                  id: crypto.randomUUID(),
                  role: 'assistant',
                  content: assistantContent,
                  timestamp: new Date().toISOString(),
                  is_voice: false,
                  thread_id: null,
                })
              }
              setIsStreaming(false)
            }
          },
        )
      } catch (e) {
        clearStream(sessionId)
        setIsStreaming(false)
        console.error('Chat stream error', e)
      }
    },
    [sessionId, addMessage, appendStream, clearStream, stream],
  )

  return (
    <div className="flex flex-col h-full">
      <MessageList messages={messages} streamingContent={currentStream} isStreaming={isStreaming} />
      <ChatInput sessionId={sessionId} onSend={handleSend} disabled={isStreaming} />
    </div>
  )
}
