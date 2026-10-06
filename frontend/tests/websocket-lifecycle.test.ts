import { afterEach, beforeEach, expect, test } from 'bun:test'
import { createLogWebSocketSession, type WsMessage } from '../src/lib/websocket'

class Socket {
  static OPEN = 1
  static instances: Socket[] = []
  readyState = 0
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  onerror: (() => void) | null = null
  onmessage: ((event: { data: string }) => void) | null = null
  sent: string[] = []
  closed = false
  constructor(public url: string) { Socket.instances.push(this) }
  send(data: string) { this.sent.push(data) }
  close() { this.closed = true; this.readyState = 3; this.onclose?.() }
  open() { this.readyState = 1; this.onopen?.() }
}
const originals = {
  WebSocket: globalThis.WebSocket,
  setTimeout: globalThis.setTimeout, clearTimeout: globalThis.clearTimeout,
  setInterval: globalThis.setInterval, clearInterval: globalThis.clearInterval,
}
let timers: Map<number, () => void>
let intervals: Map<number, () => void>
let nextId: number
beforeEach(() => {
  Socket.instances = []; timers = new Map(); intervals = new Map(); nextId = 1
  globalThis.WebSocket = Socket as unknown as typeof WebSocket
  globalThis.setTimeout = ((callback: () => void) => {
    const id = nextId++; timers.set(id, callback); return id
  }) as unknown as typeof setTimeout
  globalThis.clearTimeout = ((id: number) => timers.delete(id)) as unknown as typeof clearTimeout
  globalThis.setInterval = ((callback: () => void) => {
    const id = nextId++; intervals.set(id, callback); return id
  }) as unknown as typeof setInterval
  globalThis.clearInterval = ((id: number) => intervals.delete(id)) as unknown as typeof clearInterval
})
afterEach(() => Object.assign(globalThis, originals))

test('cleanup detaches handlers before close and ignores queued close/message/ping callbacks', () => {
  const messages: WsMessage[] = []
  const session = createLogWebSocketSession(() => 'ws://test/ws', (data) => messages.push(data))
  const ws = Socket.instances[0]; ws.open()
  const queuedClose = ws.onclose!, queuedMessage = ws.onmessage!
  const queuedPing = [...intervals.values()][0]
  session.stop(); session.stop()
  queuedClose(); queuedMessage({ data: '{"type":"lines","lines":["stale"]}' }); queuedPing()
  session.send({ type: 'clear' })
  expect(ws.closed).toBe(true)
  expect([ws.onopen, ws.onclose, ws.onerror, ws.onmessage]).toEqual([null, null, null, null])
  expect(timers.size).toBe(0); expect(intervals.size).toBe(0)
  expect(ws.sent).toEqual([]); expect(messages).toEqual([])
  expect(Socket.instances).toHaveLength(1)
})

test('cleanup while connecting ignores a queued open callback', () => {
  const session = createLogWebSocketSession(() => 'ws://test/ws', () => {})
  const queuedOpen = Socket.instances[0].onopen!
  session.stop(); queuedOpen()
  expect(intervals.size).toBe(0); expect(timers.size).toBe(0)
})

test('cleanup cancels reconnect and a queued timer cannot revive the session', () => {
  const session = createLogWebSocketSession(() => 'ws://test/ws', () => {})
  Socket.instances[0].close()
  const queuedReconnect = [...timers.values()][0]
  session.stop()
  expect(timers.size).toBe(0)
  queuedReconnect()
  expect(Socket.instances).toHaveLength(1)
})

test('reconnect uses current file; stale callbacks cannot affect the replacement connection', () => {
  let file = 'error.log'
  const messages: WsMessage[] = []
  const session = createLogWebSocketSession(() => `ws://test/ws?file=${file}`, (data) => messages.push(data))
  const old = Socket.instances[0]; old.open()
  const queuedClose = old.onclose!, queuedMessage = old.onmessage!
  const queuedPing = [...intervals.values()][0]
  file = 'access.log'; old.close()
  expect(timers.size).toBe(1)
  const [id, reconnect] = [...timers.entries()][0]; timers.delete(id); reconnect()
  const replacement = Socket.instances[1]; replacement.open()
  queuedClose(); queuedMessage({ data: '{"type":"lines","lines":["stale"]}' }); queuedPing()
  expect(replacement.url).toBe('ws://test/ws?file=access.log')
  expect(timers.size).toBe(0); expect(intervals.size).toBe(1)
  replacement.onmessage?.({ data: '{"type":"pong"}' })
  replacement.onmessage?.({ data: '{"type":"lines","lines":["current"]}' })
  session.send({ type: 'reload' })
  expect(messages).toEqual([{ type: 'lines', lines: ['current'] }])
  expect(old.sent).toEqual([]); expect(replacement.sent).toEqual(['{"type":"reload"}'])
  session.stop()
})

test('a new effect session survives callbacks from the effect it replaced', () => {
  const old = createLogWebSocketSession(() => 'ws://test/old', () => {})
  const queuedClose = Socket.instances[0].onclose!
  old.stop()
  const current = createLogWebSocketSession(() => 'ws://test/current', () => {})
  const ws = Socket.instances[1]; ws.open(); queuedClose()
  expect(ws.closed).toBe(false); expect(intervals.size).toBe(1); expect(timers.size).toBe(0)
  current.stop()
})
