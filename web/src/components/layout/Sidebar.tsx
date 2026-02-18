import { Bot } from 'lucide-react'
import { SessionList } from '../sessions/SessionList'

export function Sidebar() {
  return (
    <div className="flex flex-col h-full bg-sidebar border-r border-border w-60 shrink-0">
      {/* Logo */}
      <div className="flex items-center gap-2.5 px-4 py-4 border-b border-border">
        <div className="w-7 h-7 bg-blue-600 rounded-lg flex items-center justify-center">
          <Bot size={16} className="text-white" />
        </div>
        <span className="font-semibold text-white text-sm">CharlieBot</span>
      </div>

      {/* Session list */}
      <div className="flex-1 overflow-y-auto py-2">
        <SessionList />
      </div>
    </div>
  )
}
