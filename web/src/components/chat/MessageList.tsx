import { useEffect, useRef } from 'react'
import type { ChatMessage } from '../../types'
import { MessageItem } from './MessageItem'
import { MarkdownRenderer } from './MarkdownRenderer'
import { Spinner } from '../common/Spinner'

interface Props {
  messages: ChatMessage[]
  streamingContent: string
  isStreaming: boolean
}

export function MessageList({ messages, streamingContent, isStreaming }: Props) {
  const bottomRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages, streamingContent])

  return (
    <div className="flex-1 overflow-y-auto px-4 py-4 space-y-4">
      {messages.map((msg) => (
        <MessageItem key={msg.id} message={msg} />
      ))}

      {/* Streaming bubble */}
      {isStreaming && (
        <div className="flex gap-3">
          <div className="shrink-0 w-7 h-7 rounded-full bg-slate-600 flex items-center justify-center text-xs font-bold text-white">
            C
          </div>
          <div className="max-w-[75%] bg-slate-700 text-slate-100 rounded-2xl rounded-tl-sm px-4 py-2.5 text-sm">
            {streamingContent ? (
              <div className="prose prose-invert prose-sm max-w-none">
                <MarkdownRenderer content={streamingContent} />
              </div>
            ) : (
              <Spinner size="sm" />
            )}
          </div>
        </div>
      )}

      <div ref={bottomRef} />
    </div>
  )
}
