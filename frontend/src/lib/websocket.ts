import { useCallback, useEffect, useRef } from 'react'

export function clashWsUrl(port: string, path: string, secret?: string | null, unix?: string | null) {
  const normalizedPath = path.replace(/^\/+/, '')
  const params = new URLSearchParams()
  const useUnix = !!unix
  if (!useUnix && port) params.set('port', port)
  if (!useUnix && secret) params.set('secret', secret)
  if (useUnix && unix) params.set('unix', unix)
  const qs = params.toString()
  const protocol = location.protocol === 'https:' ? 'wss' : 'ws'
  return `${protocol}://${location.host}/clash-ws/${normalizedPath}${qs ? `?${qs}` : ''}`
}

type WsMessageHandler = (data: WsMessage) => void

export interface WsMessage {
  type: string
  lines?: string[]
  content?: string
  error?: string
}

// One effect owns one session. A stopped session never reconnects, even when a
// browser has already queued a callback before its handlers were detached.
export function createLogWebSocketSession(getUrl: () => string, onMessage: WsMessageHandler) {
  let active = true
  let socket: WebSocket | null = null
  let pingInterval: ReturnType<typeof setInterval> | null = null
  let reconnectTimeout: ReturnType<typeof setTimeout> | null = null

  const clearPing = () => {
    if (pingInterval !== null) clearInterval(pingInterval)
    pingInterval = null
  }
  const detach = (ws: WebSocket) => {
    ws.onopen = null
    ws.onclose = null
    ws.onerror = null
    ws.onmessage = null
  }
  const connect = () => {
    if (!active) return
    const ws = new WebSocket(getUrl())
    socket = ws
    const isCurrent = () => active && socket === ws

    ws.onopen = () => {
      if (!isCurrent()) return
      clearPing()
      pingInterval = setInterval(() => {
        if (isCurrent() && ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: 'ping' }))
        }
      }, 30000)
    }
    ws.onclose = () => {
      if (!isCurrent()) return
      detach(ws)
      socket = null
      clearPing()
      reconnectTimeout = setTimeout(() => {
        reconnectTimeout = null
        if (active) connect()
      }, 1000)
    }
    ws.onerror = () => {
      if (isCurrent()) ws.close()
    }
    ws.onmessage = (event) => {
      if (!isCurrent()) return
      try {
        const data = JSON.parse(event.data) as WsMessage
        if (data.type !== 'pong') onMessage(data)
      } catch {
        /* */
      }
    }
  }

  connect()
  return {
    send(data: object) {
      if (active && socket?.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify(data))
      }
    },
    stop() {
      active = false
      clearPing()
      if (reconnectTimeout !== null) clearTimeout(reconnectTimeout)
      reconnectTimeout = null
      const ws = socket
      socket = null
      if (ws) {
        detach(ws)
        ws.close()
      }
    },
  }
}

export function useWebSocket(onMessage: WsMessageHandler) {
  const sessionRef = useRef<ReturnType<typeof createLogWebSocketSession> | null>(null)
  const currentFileRef = useRef('error.log')

  useEffect(() => {
    const session = createLogWebSocketSession(
      () => `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws?file=${currentFileRef.current}`,
      onMessage
    )
    sessionRef.current = session
    return () => {
      sessionRef.current = null
      session.stop()
    }
  }, [onMessage])

  const send = useCallback((data: object) => sessionRef.current?.send(data), [])

  const switchFile = useCallback(
    (filename: string) => {
      currentFileRef.current = filename
      send({ type: 'switchFile', file: filename })
    },
    [send]
  )

  const applyFilter = useCallback(
    (filter: string) => {
      if (!filter.trim()) {
        send({ type: 'reload' })
      } else {
        send({ type: 'filter', query: filter })
      }
    },
    [send]
  )

  const clearLog = useCallback(() => send({ type: 'clear' }), [send])
  const reload = useCallback((filter?: string) => send({ type: 'reload', query: filter ?? '' }), [send])

  return { switchFile, applyFilter, clearLog, reload }
}
