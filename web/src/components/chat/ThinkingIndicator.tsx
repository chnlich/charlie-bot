import { useEffect, useRef, useState } from 'react'

/**
 * Animated "Thinking … Xs" indicator shown while the master agent is processing.
 * The timer resets each time the component mounts (i.e. when streaming starts).
 */
export function ThinkingIndicator() {
  const [elapsed, setElapsed] = useState(0)
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null)

  useEffect(() => {
    intervalRef.current = setInterval(() => {
      setElapsed((prev) => prev + 1)
    }, 1000)
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current)
    }
  }, [])

  return (
    <div className="flex items-center gap-2 text-sm text-slate-300">
      <span className="inline-block h-2 w-2 rounded-full bg-blue-400 animate-pulse" />
      <span>Thinking&hellip; {elapsed}s</span>
    </div>
  )
}
