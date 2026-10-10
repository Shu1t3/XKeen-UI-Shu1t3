import { expect, test } from 'bun:test'
import { fanOutRouters, mapConcurrent, ROUTER_CONCURRENCY_LIMIT } from '../src/lib/api'
import { refreshAllOnline } from '../src/lib/multi-routers/actions'
import { routerId } from '../src/lib/multi-routers/model'
import { useRoutersStore } from '../src/lib/multi-routers/store'

test('mapConcurrent respects concurrency limit and preserves output order', async () => {
  const items = Array.from({ length: 25 }, (_, i) => i)
  const concurrency = 4
  let active = 0
  let maxActive = 0

  const results = await mapConcurrent(items, concurrency, async (item) => {
    active++
    if (active > maxActive) maxActive = active
    await new Promise((resolve) => setTimeout(resolve, 5))
    active--
    return item * 2
  })

  expect(maxActive).toBeLessThanOrEqual(concurrency)
  expect(active).toBe(0)
  expect(results).toEqual(items.map((i) => i * 2))
})

test('mapConcurrent handles empty items and single item properly', async () => {
  expect(await mapConcurrent([], 5, async (x) => x)).toEqual([])
  expect(await mapConcurrent([42], 5, async (x) => x + 1)).toEqual([43])
})

test('fanOutRouters bounds in-flight requests and collects individual errors', async () => {
  const targetIds = Array.from({ length: 18 }, (_, i) => `router-${i}`)
  let active = 0
  let maxActive = 0
  const limit = 5

  const results = await fanOutRouters(
    targetIds,
    async (id) => {
      active++
      if (active > maxActive) maxActive = active
      await new Promise((resolve) => setTimeout(resolve, 5))
      active--
      if (id === 'router-3') throw new Error('Network timeout')
    },
    (id) => `http://192.168.1.${id}:8080`,
    limit
  )

  expect(maxActive).toBeLessThanOrEqual(limit)
  expect(active).toBe(0)
  expect(results.length).toBe(18)
  expect(results[3]).toEqual({ id: 'router-3', ok: false, error: 'Network timeout' })
  expect(results[0]).toEqual({ id: 'router-0', ok: true })
})

test('refreshAllOnline runs with limited concurrency across large fleet', async () => {
  const fleet = Array.from({ length: 20 }, (_, i) => ({
    host: `192.168.2.${i + 10}`,
    port: 8080,
    name: `Router-${i}`,
  }))

  useRoutersStore.setState({
    routers: fleet,
    applyTargets: fleet.map(routerId),
    online: {},
    auth: {},
    commandStatus: {},
  })

  let active = 0
  let maxActive = 0
  const limit = 4

  const originalFetch = globalThis.fetch
  try {
    globalThis.fetch = (async () => {
      active++
      if (active > maxActive) maxActive = active
      await new Promise((resolve) => setTimeout(resolve, 4))
      active--
      return Response.json({ success: true, enabled: false })
    }) as typeof fetch

    await refreshAllOnline(limit)

    expect(maxActive).toBeLessThanOrEqual(limit)
    expect(active).toBe(0)
    expect(ROUTER_CONCURRENCY_LIMIT).toBe(6)
  } finally {
    globalThis.fetch = originalFetch
  }
})
