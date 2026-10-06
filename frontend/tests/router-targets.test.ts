import { afterEach, beforeEach, expect, test } from 'bun:test'
import { apiCall, fanOutRouters } from '../src/lib/api'
import { filterAuthBlockedTargets, refreshAllOnline, runMassTask, snapshotMassTargets } from '../src/lib/multi-routers/actions'
import { routerId } from '../src/lib/multi-routers/model'
import { getBaseUrlForId, useRoutersStore } from '../src/lib/multi-routers/store'
const remote = { host: '192.0.2.1', port: 1000, name: 'Remote' }
const id = routerId(remote)
const originalFetch = globalThis.fetch
let requests: string[]
beforeEach(() => {
  requests = []
  useRoutersStore.setState({ routers: [{ ...remote }], applyTargets: [id], online: { local: true, [id]: true }, auth: {}, commandStatus: {} })
  globalThis.fetch = (async (input) => {
    requests.push(String(input))
    return Response.json({ success: true, enabled: false })
  }) as typeof fetch
})
afterEach(() => { globalThis.fetch = originalFetch })
const writeConfig = async (_id: string, baseUrl: string | null) => {
  await apiCall('PUT', 'configs', { file: '/test', content: '{}' }, { baseUrl })
}
test('empty selection and deletion never select local', async () => {
  useRoutersStore.getState().setApplyTargets([])
  expect(() => snapshotMassTargets(true)).toThrow('Выберите')
  useRoutersStore.setState({ applyTargets: [id] })
  useRoutersStore.getState().setRouters([])
  expect(useRoutersStore.getState().applyTargets).toEqual([])
  expect(() => snapshotMassTargets(true)).toThrow('Выберите')
  await expect(runMassTask([], writeConfig)).rejects.toThrow('Выберите')
  expect(requests).toEqual([])
})
test('offline refresh clears selection without fallback', async () => {
  globalThis.fetch = (async (input) => {
    requests.push(String(input))
    return Response.json({ success: String(input).startsWith('/api'), enabled: false })
  }) as typeof fetch
  await refreshAllOnline()
  expect(useRoutersStore.getState().applyTargets).toEqual([])
  expect(() => snapshotMassTargets(true)).toThrow('Выберите')
})
test('unknown remote fails without calling local API', async () => {
  expect(() => getBaseUrlForId('missing')).toThrow('не существует')
  expect((await fanOutRouters(['missing'], writeConfig, () => null))[0].ok).toBe(false)
  expect(requests).toEqual([])
})
test('removal after confirmation records error without any request', async () => {
  const targets = snapshotMassTargets(true)
  useRoutersStore.getState().setRouters([])
  expect(await runMassTask(targets, writeConfig)).toMatchObject([{ id, ok: false }])
  expect(useRoutersStore.getState().commandStatus[id].status).toBe('error')
  expect(requests).toEqual([])
})
test('selection changes during auth probe retain frozen original target', async () => {
  const targets = snapshotMassTargets(true)
  const filtering = filterAuthBlockedTargets(targets)
  useRoutersStore.getState().setApplyTargets(['local'])
  useRoutersStore.getState().setRouters([{ ...remote, name: 'Renamed' }])
  const { allowed } = await filtering
  expect(Object.isFrozen(targets)).toBe(true)
  expect(Object.isFrozen(targets[0])).toBe(true)
  expect(allowed[0].label).toBe('Remote')
  expect((await runMassTask(allowed, writeConfig))[0].ok).toBe(true)
  expect(requests).toEqual(['http://192.0.2.1:1000/api/auth/login', 'http://192.0.2.1:1000/api/configs'])
})
test('address mismatch rejects mutation', async () => {
  const targets = snapshotMassTargets(true)
  expect((await runMassTask([{ ...targets[0], baseUrl: 'http://192.0.2.2:1000' }], writeConfig))[0].ok).toBe(false)
  expect(requests).toEqual([])
})
test('local execution requires explicit selection or disabled multi-router', async () => {
  useRoutersStore.getState().setApplyTargets(['local'])
  expect((await runMassTask(snapshotMassTargets(true), writeConfig))[0].ok).toBe(true)
  useRoutersStore.getState().setApplyTargets([])
  expect((await runMassTask(snapshotMassTargets(false), writeConfig))[0].ok).toBe(true)
  expect(requests).toEqual(['/api/configs', '/api/configs'])
})
test('removal while auth probe is pending cannot turn the captured remote into local', async () => {
  let release!: () => void
  globalThis.fetch = (async (input) => {
    requests.push(String(input))
    await new Promise<void>((resolve) => { release = resolve })
    return Response.json({ enabled: false })
  }) as typeof fetch
  const filtering = filterAuthBlockedTargets(snapshotMassTargets(true))
  useRoutersStore.getState().setRouters([])
  release()
  const { allowed } = await filtering
  expect((await runMassTask(allowed, writeConfig))[0].ok).toBe(false)
  expect(requests).toEqual(['http://192.0.2.1:1000/api/auth/login'])
})
test('mixed selection executes only the explicit surviving local target', async () => {
  useRoutersStore.getState().setApplyTargets(['local', id])
  const targets = snapshotMassTargets(true)
  useRoutersStore.getState().setRouters([])
  expect(await runMassTask(targets, writeConfig)).toMatchObject([{ id: 'local', ok: true }, { id, ok: false }])
  expect(requests).toEqual(['/api/configs'])
})
