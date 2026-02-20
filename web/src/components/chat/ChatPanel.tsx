import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useWebSocket } from '../../hooks/useWebSocket'
import { useChatStore } from '../../store/chat'
import { useSessionsStore } from '../../store/sessions'
import { sessionsApi } from '../../api/sessions'
import type { ChatMessage } from '../../types'
import { ChatInput } from './ChatInput'
import { MessageList } from './MessageList'

interface Props {
  sessionId: string
}

/** Extract text from an assistant-type CC event's message.content blocks. */
function extractAssistantText(event: Record<string, unknown>): string {
  const msg = event.message as Record<string, unknown> | undefined
  if (!msg) return ''
  const blocks = msg.content as Array<Record<string, unknown>> | undefined
  if (!Array.isArray(blocks)) return ''
  return blocks
    .filter((b) => b.type === 'text')
    .map((b) => b.text as string)
    .join('')
}

export function ChatPanel({ sessionId }: Props) {
  const [isStreaming, setIsStreaming] = useState(false)
  const catchupDoneRef = useRef(false)
  const { messagesBySession, addMessage, setMessages, appendStream, clearStream } =
    useChatStore()

  const messages = messagesBySession[sessionId] ?? []

  // Derive the thinking start time from the last user message timestamp.
  // This survives page refreshes and session switches because messages are
  // replayed from persisted chat_events.jsonl during WebSocket catch-up.
  const thinkingStartedAt = useMemo(() => {
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].role === 'user') return messages[i].timestamp
    }
    return null
  }, [messages])

  // Clear unread when this session becomes active
  useEffect(() => {
    useSessionsStore.getState().markSessionRead(sessionId)
    sessionsApi.markRead(sessionId).catch(console.error)
  }, [sessionId])

  // Handle WebSocket messages (master CC events + worker summaries)
  const handleWsMessage = useCallback(
    (data: unknown) => {
      const event = data as Record<string, unknown>
      const type = event.type as string

      if (type === 'catchup_complete') {
        catchupDoneRef.current = true
        // Finalize any streaming content accumulated during catch-up
        const content = clearStream(sessionId)
        if (content.trim()) {
          addMessage(sessionId, {
            id: crypto.randomUUID(),
            role: 'assistant',
            content,
            timestamp: new Date().toISOString(),
            is_voice: false,
            thread_id: null,
          })
        }
        return
      }

      if (type === 'ping') return

      if (type === 'user') {
        // Our custom user events have a top-level string `content`.
        // Claude Code tool-result events also have type "user" but carry
        // `message` instead — skip those.
        if (!catchupDoneRef.current && typeof event.content === 'string') {
          addMessage(sessionId, {
            id: crypto.randomUUID(),
            role: 'user',
            content: event.content as string,
            timestamp: (event.timestamp as string) || new Date().toISOString(),
            is_voice: false,
            thread_id: null,
          })
        }
        return
      }

      if (type === 'assistant') {
        const text = extractAssistantText(event)
        if (text) {
          appendStream(sessionId, text)
          if (!isStreaming) setIsStreaming(true)
        }
        return
      }

      if (type === 'master_done') {
        const content = clearStream(sessionId)
        if (content.trim()) {
          addMessage(sessionId, {
            id: crypto.randomUUID(),
            role: 'assistant',
            content,
            timestamp: new Date().toISOString(),
            is_voice: false,
            thread_id: null,
          })
        }
        setIsStreaming(false)
        return
      }

      if (type === 'task_delegated') {
        // Flush any pending streaming content first
        const content = clearStream(sessionId)
        if (content.trim()) {
          addMessage(sessionId, {
            id: crypto.randomUUID(),
            role: 'assistant',
            content,
            timestamp: new Date().toISOString(),
            is_voice: false,
            thread_id: null,
          })
        }
        const systemMsg: ChatMessage = {
          id: crypto.randomUUID(),
          role: 'system',
          content: `Task queued [${event.priority}]: ${event.description}`,
          timestamp: new Date().toISOString(),
          is_voice: false,
          thread_id: (event.task_id as string) ?? null,
        }
        addMessage(sessionId, systemMsg)
        return
      }

      if (type === 'worker_summary') {
        addMessage(sessionId, {
          id: crypto.randomUUID(),
          role: 'assistant',
          content: event.content as string,
          timestamp: new Date().toISOString(),
          is_voice: false,
          thread_id: (event.thread_id as string) ?? null,
        })
        return
      }
    },
    [sessionId, addMessage, appendStream, clearStream, isStreaming],
  )

  // Connect to the session WebSocket — handles both master CC events and worker summaries
  useWebSocket(`/ws/sessions/${sessionId}`, {
    onMessage: handleWsMessage,
    enabled: true,
  })

  // Reset state when session changes
  useEffect(() => {
    catchupDoneRef.current = false
    setMessages(sessionId, [])
    clearStream(sessionId)
    setIsStreaming(false)
  }, [sessionId, setMessages, clearStream])

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

      // Fire-and-forget POST — response streams via WebSocket
      try {
        await fetch(`/api/chat/${sessionId}/message`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ content }),
        })
      } catch (e) {
        console.error('Failed to send message', e)
        setIsStreaming(false)
      }
    },
    [sessionId, addMessage],
  )

  return (
    <div className="flex flex-col h-full">
      <MessageList messages={messages} isStreaming={isStreaming} thinkingStartedAt={thinkingStartedAt} />
      <ChatInput sessionId={sessionId} onSend={handleSend} disabled={isStreaming} />
    </div>
  )
}
