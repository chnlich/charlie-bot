import { useState } from 'react'
import { useSessionsStore } from '../../store/sessions'
import { Sidebar } from './Sidebar'
import { Header } from './Header'
import { ChatPanel } from '../chat/ChatPanel'
import { ThreadsPanel } from '../threads/ThreadsPanel'
import { QueuePanel } from '../queue/QueuePanel'

export function AppShell() {
  const { sessions, activeSessionId } = useSessionsStore()
  const [showThreads, setShowThreads] = useState(true)
  const [showQueue, setShowQueue] = useState(false)

  const activeSession = sessions.find((s) => s.id === activeSessionId) ?? null

  return (
    <div className="flex h-screen bg-slate-900 text-slate-100 overflow-hidden">
      <Sidebar />

      <div className="flex flex-col flex-1 min-w-0">
        <Header
          session={activeSession}
          showThreads={showThreads}
          showQueue={showQueue}
          onToggleThreads={() => setShowThreads((v) => !v)}
          onToggleQueue={() => setShowQueue((v) => !v)}
        />

        <div className="flex flex-1 min-h-0">
          {/* Main chat area */}
          <div className="flex-1 min-w-0">
            {activeSessionId ? (
              <ChatPanel sessionId={activeSessionId} />
            ) : (
              <div className="flex items-center justify-center h-full text-slate-600 text-sm">
                Select or create a session to get started
              </div>
            )}
          </div>

          {/* Right panel: Threads or Queue */}
          {activeSessionId && (showThreads || showQueue) && (
            <div className="w-80 shrink-0 border-l border-border bg-surface">
              {showQueue && !showThreads ? (
                <QueuePanel sessionId={activeSessionId} />
              ) : (
                <ThreadsPanel sessionId={activeSessionId} />
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
