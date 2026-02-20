import { useEffect, useRef } from 'react'
import type { ChatMessage } from '../../types'
import { MessageItem } from './MessageItem'
import { ThinkingIndicator } from './ThinkingIndicator'

interface Props {
  messages: ChatMessage[]
  thinkingStartedAt: string | null
}

export function MessageList({ messages, thinkingStartedAt }: Props) {
  const bottomRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  return (
    <div className="flex-1 overflow-y-auto px-4 py-4 space-y-4">
      {messages.map((msg) => (
        <MessageItem key={msg.id} message={msg} />
      ))}

      {/* Thinking indicator — driven by backend thinking_since timestamp */}
      {thinkingStartedAt && (
        <div className="flex gap-3">
          <div className="shrink-0 w-7 h-7 rounded-full bg-slate-600 flex items-center justify-center text-xs font-bold text-white">
            C
          </div>
          <div className="max-w-[75%] bg-slate-700 text-slate-100 rounded-2xl rounded-tl-sm px-4 py-2.5 text-sm">
            <ThinkingIndicator startedAt={thinkingStartedAt} />
          </div>
        </div>
      )}

      <div ref={bottomRef} />
    </div>
  )
}
