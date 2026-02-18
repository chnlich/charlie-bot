import { useState } from 'react'
import type { ThreadMetadata } from '../../types'
import { threadsApi } from '../../api/threads'
import { useThreadsStore } from '../../store/threads'
import { PlanStep } from './PlanStep'

interface Props {
  thread: ThreadMetadata
  sessionId: string
}

export function PlanReview({ thread, sessionId }: Props) {
  // Extract plan steps from events (type=assistant with numbered list content)
  const { eventsByThread, updateThread } = useThreadsStore()
  const events = eventsByThread[thread.id] ?? []

  // Build initial steps from thread plan_steps or extract from events
  const initialSteps = thread.plan_steps ?? extractStepsFromEvents(events)
  const [steps, setSteps] = useState(initialSteps)
  const [checked, setChecked] = useState<boolean[]>(initialSteps.map(() => true))
  const [loading, setLoading] = useState(false)

  const toggleStep = (i: number) => setChecked((prev) => prev.map((v, idx) => (idx === i ? !v : v)))
  const editStep = (i: number, text: string) =>
    setSteps((prev) => prev.map((s, idx) => (idx === i ? text : s)))

  const handleApprove = async () => {
    const approved = steps.filter((_, i) => checked[i])
    if (approved.length === 0) return
    setLoading(true)
    try {
      await threadsApi.approvePlan(sessionId, thread.id, approved)
      updateThread({ ...thread, status: 'completed' })
    } catch (e) {
      console.error('Plan approval failed', e)
    } finally {
      setLoading(false)
    }
  }

  if (steps.length === 0) {
    return (
      <div className="bg-yellow-500/10 border border-yellow-500/20 rounded-lg p-3 text-xs text-yellow-300">
        Plan generated. Waiting for steps to extract from worker output...
      </div>
    )
  }

  return (
    <div className="bg-slate-800 border border-border rounded-lg p-3">
      <h3 className="text-xs font-semibold text-slate-300 mb-2">Plan Review</h3>
      <p className="text-xs text-slate-500 mb-3">
        Check steps to include, edit if needed, then approve.
      </p>

      <div className="space-y-0 divide-y divide-border/30">
        {steps.map((step, i) => (
          <PlanStep key={i} step={step} index={i} checked={checked[i]} onToggle={toggleStep} onEdit={editStep} />
        ))}
      </div>

      <div className="flex gap-2 mt-3">
        <button
          onClick={handleApprove}
          disabled={loading || !checked.some(Boolean)}
          className="flex-1 bg-green-600 hover:bg-green-700 disabled:opacity-50 text-white rounded py-1.5 text-xs font-medium"
        >
          {loading ? 'Scheduling...' : `Approve & Run (${checked.filter(Boolean).length} steps)`}
        </button>
      </div>
    </div>
  )
}

function extractStepsFromEvents(events: { type: string; content?: string }[]): string[] {
  // Look for assistant content events with numbered lists
  const lines: string[] = []
  for (const event of events) {
    if (event.type === 'assistant' && event.content) {
      const matches = event.content.match(/^\d+\.\s+.+/gm)
      if (matches) lines.push(...matches.map((m) => m.replace(/^\d+\.\s+/, '')))
    }
  }
  return lines
}
