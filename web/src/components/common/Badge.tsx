import { clsx } from 'clsx'
import type { Priority, TaskStatus, ThreadStatus } from '../../types'

interface BadgeProps {
  label: string
  variant?: 'default' | 'priority' | 'status'
  className?: string
}

const PRIORITY_COLORS: Record<Priority, string> = {
  P0: 'bg-red-500/20 text-red-300 border border-red-500/30',
  P1: 'bg-yellow-500/20 text-yellow-300 border border-yellow-500/30',
  P2: 'bg-slate-500/20 text-slate-300 border border-slate-500/30',
}

const STATUS_COLORS: Record<string, string> = {
  pending: 'bg-slate-500/20 text-slate-300',
  running: 'bg-blue-500/20 text-blue-300',
  completed: 'bg-green-500/20 text-green-300',
  failed: 'bg-red-500/20 text-red-300',
  pending_quota: 'bg-orange-500/20 text-orange-300',
  cancelled: 'bg-slate-600/20 text-slate-400',
  idle: 'bg-slate-500/20 text-slate-300',
  planning: 'bg-purple-500/20 text-purple-300',
  awaiting_approval: 'bg-yellow-500/20 text-yellow-300',
  conflict: 'bg-red-500/20 text-red-300',
}

export function PriorityBadge({ priority }: { priority: Priority }) {
  return (
    <span className={clsx('text-xs font-mono px-1.5 py-0.5 rounded', PRIORITY_COLORS[priority])}>
      {priority}
    </span>
  )
}

export function StatusBadge({ status }: { status: string }) {
  return (
    <span className={clsx('text-xs px-1.5 py-0.5 rounded capitalize', STATUS_COLORS[status] ?? 'bg-slate-500/20 text-slate-300')}>
      {status.replace('_', ' ')}
    </span>
  )
}

export function Badge({ label, className }: BadgeProps) {
  return (
    <span className={clsx('text-xs px-1.5 py-0.5 rounded bg-slate-500/20 text-slate-300', className)}>
      {label}
    </span>
  )
}
