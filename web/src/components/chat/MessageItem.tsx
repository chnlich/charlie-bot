import { clsx } from 'clsx'
import { Mic } from 'lucide-react'
import type { ChatMessage } from '../../types'
import { useDebugStore } from '../../store/debug'
import { MarkdownRenderer } from './MarkdownRenderer'

interface Props {
  message: ChatMessage
}

export function MessageItem({ message }: Props) {
  const debugMode = useDebugStore((s) => s.debugMode)
  const isUser = message.role === 'user'
  const isSystem = message.role === 'system'

  if (isSystem) {
    return (
      <div className="flex justify-center my-2">
        <span className="text-xs text-slate-500 bg-slate-800 px-3 py-1 rounded-full">
          {message.content}
        </span>
      </div>
    )
  }

  return (
    <div className={clsx('flex gap-3', isUser ? 'flex-row-reverse' : 'flex-row')}>
      {/* Avatar */}
      <div
        className={clsx(
          'shrink-0 w-7 h-7 rounded-full flex items-center justify-center text-xs font-bold',
          isUser ? 'bg-blue-600 text-white' : 'bg-slate-600 text-white',
        )}
      >
        {isUser ? 'U' : 'C'}
      </div>

      {/* Bubble */}
      <div
        className={clsx(
          'max-w-[75%] rounded-2xl px-4 py-2.5 text-sm',
          isUser
            ? 'bg-blue-600 text-white rounded-tr-sm'
            : 'bg-slate-700 text-slate-100 rounded-tl-sm',
        )}
      >
        {message.is_voice && (
          <div className="flex items-center gap-1 text-xs opacity-70 mb-1">
            <Mic size={10} />
            <span>Voice message</span>
          </div>
        )}
        {isUser ? (
          <p className="whitespace-pre-wrap">{message.content}</p>
        ) : (
          <div className="prose prose-invert prose-sm max-w-none">
            <MarkdownRenderer content={message.content} />
          </div>
        )}
      </div>

      {/* Debug footer */}
      {debugMode && message.thread_id && (
        <div className={clsx('text-[10px] text-slate-600 mt-0.5 font-mono', isUser ? 'text-right' : 'text-left')}>
          thread: {message.thread_id} | msg: {message.id}
        </div>
      )}
    </div>
  )
}
