import { useEffect } from 'react'
import { sessionsApi } from '../../api/sessions'
import { useThreadsStore } from '../../store/threads'
import { ThreadItem } from './ThreadItem'
import { ThreadDetail } from './ThreadDetail'

interface Props {
  sessionId: string
}

export function ThreadsPanel({ sessionId }: Props) {
  const { threadsBySession, activeThreadId, setThreads, setActiveThread } = useThreadsStore()
  const threads = threadsBySession[sessionId] ?? []
  const activeThread = threads.find((t) => t.id === activeThreadId) ?? null

  useEffect(() => {
    sessionsApi.listThreads(sessionId).then((ts) => setThreads(sessionId, ts)).catch(console.error)
  }, [sessionId, setThreads])

  if (activeThread) {
    return (
      <ThreadDetail
        thread={activeThread}
        sessionId={sessionId}
        onClose={() => setActiveThread(null)}
      />
    )
  }

  return (
    <div className="flex flex-col h-full">
      <div className="px-4 py-3 border-b border-border">
        <h2 className="text-sm font-semibold text-slate-300">Workers</h2>
        <p className="text-xs text-slate-500">{threads.length} thread{threads.length !== 1 ? 's' : ''}</p>
      </div>

      <div className="flex-1 overflow-y-auto p-3 space-y-2">
        {threads.length === 0 && (
          <p className="text-xs text-slate-600 text-center py-4">
            No workers yet. Send a task to get started.
          </p>
        )}
        {threads.map((thread) => (
          <ThreadItem
            key={thread.id}
            thread={thread}
            sessionId={sessionId}
            active={thread.id === activeThreadId}
            onClick={() => setActiveThread(thread.id)}
          />
        ))}
      </div>
    </div>
  )
}
