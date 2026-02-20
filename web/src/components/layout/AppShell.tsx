import { useState } from 'react'
import { useSessionsStore } from '../../store/sessions'
import { Sidebar } from './Sidebar'
import { Header } from './Header'
import { ChatPanel } from '../chat/ChatPanel'
import { ThreadsPanel } from '../threads/ThreadsPanel'
import { QueuePanel } from '../queue/QueuePanel'

export function AppShell() {
  const { sessions, activeSessionId } = useSessionsStore()
  const [showThreads, setShowThreads] = useState(() => window.matchMedia('(min-width: 1280px)').matches)
  const [showQueue, setShowQueue] = useState(false)
  const [sidebarOpen, setSidebarOpen] = useState(false)

  const activeSession = sessions.find((s) => s.id === activeSessionId) ?? null

  return (
    <div className="flex h-dvh bg-slate-900 text-slate-100 overflow-hidden">
      {/* Sidebar: always visible on lg+, overlay on smaller screens */}
      <div className="hidden lg:block shrink-0">
        <Sidebar />
      </div>
      {sidebarOpen && (
        <>
          <div className="fixed inset-0 z-40 bg-black/50 lg:hidden" onClick={() => setSidebarOpen(false)} />
          <div className="fixed inset-y-0 left-0 z-50 lg:hidden">
            <Sidebar />
          </div>
        </>
      )}

      <div className="flex flex-col flex-1 min-w-0">
        <Header
          session={activeSession}
          showThreads={showThreads}
          showQueue={showQueue}
          onToggleThreads={() => setShowThreads((v) => !v)}
          onToggleQueue={() => setShowQueue((v) => !v)}
          onToggleSidebar={() => setSidebarOpen((v) => !v)}
        />

        <div className="flex flex-1 min-h-0">
          {/* Main chat area */}
          <div className="flex-1 min-w-0">
            {activeSessionId ? (
              <ChatPanel key={activeSessionId} sessionId={activeSessionId} />
            ) : (
              <div className="flex items-center justify-center h-full text-slate-600 text-sm">
                Select or create a session to get started
              </div>
            )}
          </div>

          {/* Right panel: Threads or Queue — inline on xl+, overlay on smaller */}
          {activeSessionId && (showThreads || showQueue) && (
            <>
              {/* Desktop inline panel */}
              <div className="hidden xl:block w-80 shrink-0 border-l border-border bg-surface">
                {showQueue && !showThreads ? (
                  <QueuePanel sessionId={activeSessionId} />
                ) : (
                  <ThreadsPanel sessionId={activeSessionId} />
                )}
              </div>
              {/* Mobile/tablet overlay panel */}
              <div className="xl:hidden fixed inset-y-0 right-0 z-40 w-80 max-w-[85vw] border-l border-border bg-surface shadow-xl">
                {showQueue && !showThreads ? (
                  <QueuePanel sessionId={activeSessionId} />
                ) : (
                  <ThreadsPanel sessionId={activeSessionId} />
                )}
              </div>
              <div
                className="xl:hidden fixed inset-0 z-30 bg-black/50"
                onClick={() => { setShowThreads(false); setShowQueue(false) }}
              />
            </>
          )}
        </div>
      </div>
    </div>
  )
}
