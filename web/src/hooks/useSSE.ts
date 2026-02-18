import { useCallback, useRef } from 'react'

type SSEHandler = (event: unknown) => void

export function useSSE() {
  const abortRef = useRef<AbortController | null>(null)

  const stream = useCallback(async (url: string, body: unknown, onEvent: SSEHandler) => {
    abortRef.current?.abort()
    const controller = new AbortController()
    abortRef.current = controller

    const res = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: controller.signal,
    })

    if (!res.ok || !res.body) throw new Error(`SSE request failed: ${res.status}`)

    const reader = res.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''

    while (true) {
      const { done, value } = await reader.read()
      if (done) break

      buffer += decoder.decode(value, { stream: true })
      const lines = buffer.split('\n')
      buffer = lines.pop() ?? ''

      for (const line of lines) {
        if (line.startsWith('data: ')) {
          const data = line.slice(6).trim()
          if (data) {
            try {
              onEvent(JSON.parse(data))
            } catch {
              onEvent(data)
            }
          }
        }
      }
    }
  }, [])

  const abort = useCallback(() => {
    abortRef.current?.abort()
  }, [])

  return { stream, abort }
}
