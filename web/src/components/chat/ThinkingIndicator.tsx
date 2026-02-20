import { useEffect, useRef, useState } from 'react'

interface Props {
  /** ISO-8601 timestamp of when the agent started thinking. */
  startedAt: string
}

/**
 * Animated "Thinking … Xs" indicator shown while the master agent is processing.
 * Computes elapsed time from the provided start timestamp so the counter
 * survives page refreshes and session switches.
 */
export function ThinkingIndicator({ startedAt }: Props) {
  const computeElapsed = () => Math.max(0, Math.floor((Date.now() - new Date(startedAt).getTime()) / 1000))
  const [elapsed, setElapsed] = useState(computeElapsed)
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null)

  useEffect(() => {
    setElapsed(computeElapsed())
    intervalRef.current = setInterval(() => {
      setElapsed(computeElapsed())
    }, 1000)
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current)
    }
  }, [startedAt])

  return (
    <div className="flex items-center gap-2 text-sm text-slate-300">
      <span className="inline-block h-2 w-2 rounded-full bg-blue-400 animate-pulse" />
      <span>Thinking&hellip; {elapsed}s</span>
    </div>
  )
}
