import { useCallback, useEffect, useRef, useState } from 'react'

type WsStatus = 'connecting' | 'open' | 'closed' | 'error'

interface UseWebSocketOptions {
  onMessage: (data: unknown) => void
  onOpen?: () => void
  onClose?: () => void
  enabled?: boolean
}

const BASE_DELAY = 1000
const MAX_DELAY = 30000

export function useWebSocket(url: string, options: UseWebSocketOptions) {
  const [status, setStatus] = useState<WsStatus>('closed')
  const wsRef = useRef<WebSocket | null>(null)
  const retryDelay = useRef(BASE_DELAY)
  const retryTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const mountedRef = useRef(true)
  const optionsRef = useRef(options)
  optionsRef.current = options

  const connect = useCallback(() => {
    if (!mountedRef.current) return
    if (!optionsRef.current.enabled) return

    const wsUrl = `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}${url}`
    setStatus('connecting')
    const ws = new WebSocket(wsUrl)
    wsRef.current = ws

    ws.onopen = () => {
      if (!mountedRef.current) return
      setStatus('open')
      retryDelay.current = BASE_DELAY
      optionsRef.current.onOpen?.()
    }

    ws.onmessage = (e) => {
      if (!mountedRef.current) return
      try {
        const data = JSON.parse(e.data)
        optionsRef.current.onMessage(data)
      } catch {
        optionsRef.current.onMessage(e.data)
      }
    }

    ws.onclose = () => {
      if (!mountedRef.current) return
      setStatus('closed')
      optionsRef.current.onClose?.()
      // Exponential backoff reconnect
      retryTimer.current = setTimeout(() => {
        if (mountedRef.current && optionsRef.current.enabled) {
          retryDelay.current = Math.min(retryDelay.current * 2, MAX_DELAY)
          connect()
        }
      }, retryDelay.current)
    }

    ws.onerror = () => {
      setStatus('error')
      ws.close()
    }
  }, [url])

  useEffect(() => {
    mountedRef.current = true
    if (options.enabled !== false) {
      connect()
    }
    return () => {
      mountedRef.current = false
      if (retryTimer.current) clearTimeout(retryTimer.current)
      wsRef.current?.close()
    }
  }, [url, options.enabled, connect])

  const send = useCallback((data: unknown) => {
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify(data))
    }
  }, [])

  return { status, send }
}
