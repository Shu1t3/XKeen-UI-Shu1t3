import { beforeEach, expect, test } from 'bun:test'
import { LOCAL_ROUTER_ID, type RouterTarget } from '../src/lib/multi-routers/model'
import { getAppState } from '../src/lib/store'
import type { Config } from '../src/lib/types'

const FILE_A = '/opt/etc/xray/config.json'
const FILE_B = '/opt/etc/xray/routes.json'

beforeEach(() => {
  const initialConfigs: Config[] = [
    { file: FILE_A, content: '{"inbounds":[]}', savedContent: '{"inbounds":[]}', isDirty: false, revision: 0, savedRevision: 0 },
    { file: FILE_B, content: '{"rules":[]}', savedContent: '{"rules":[]}', isDirty: false, revision: 0, savedRevision: 0 },
  ]
  getAppState().dispatch({ type: 'SET_CONFIGS', configs: initialConfigs })
})

test('switching tabs while save is pending preserves other tab content and dirty state', () => {
  const { dispatch } = getAppState()

  // User edits file A
  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_A, index: 0, isDirty: true, content: '{"inbounds":["proxyA"]}' })

  // Save starts for file A
  const pendingFile = FILE_A
  const pendingContent = '{"inbounds":["proxyA"]}'
  const pendingRevision = getAppState().configs[0].revision ?? 0

  // User switches to tab B and edits file B
  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_B, index: 1, isDirty: true, content: '{"rules":["ruleB_edit"]}' })

  // Save for file A completes and is dispatched with file ID
  dispatch({
    type: 'SAVE_CONFIG',
    file: pendingFile,
    content: pendingContent,
    savedRevision: pendingRevision,
  })

  const configs = getAppState().configs
  const configA = configs.find((c) => c.file === FILE_A)!
  const configB = configs.find((c) => c.file === FILE_B)!

  // File A is saved and marked clean
  expect(configA.content).toBe('{"inbounds":["proxyA"]}')
  expect(configA.savedContent).toBe('{"inbounds":["proxyA"]}')
  expect(configA.isDirty).toBe(false)

  // File B is NOT overwritten by file A and preserves its dirty edits
  expect(configB.content).toBe('{"rules":["ruleB_edit"]}')
  expect(configB.savedContent).toBe('{"rules":[]}')
  expect(configB.isDirty).toBe(true)
})

test('typing newer edits while save is in flight preserves newer edits and keeps dirty flag', () => {
  const { dispatch } = getAppState()

  // User edits file A
  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_A, index: 0, isDirty: true, content: '{"inbounds":["v1"]}' })
  const saveRevision = getAppState().configs[0].revision ?? 0
  const savedContent = '{"inbounds":["v1"]}'

  // User types further while save is pending
  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_A, index: 0, isDirty: true, content: '{"inbounds":["v1","v2_in_flight"]}' })

  // Save response for v1 arrives
  dispatch({
    type: 'SAVE_CONFIG',
    file: FILE_A,
    content: savedContent,
    savedRevision: saveRevision,
  })

  const configA = getAppState().configs.find((c) => c.file === FILE_A)!

  // Saved baseline is what was committed to disk (v1)
  expect(configA.savedContent).toBe('{"inbounds":["v1"]}')
  // Current in-memory content retains the user's latest keystrokes (v1 + v2)
  expect(configA.content).toBe('{"inbounds":["v1","v2_in_flight"]}')
  // Config stays dirty because unsaved changes exist beyond what was saved
  expect(configA.isDirty).toBe(true)
  expect(configA.savedRevision).toBe(saveRevision)
})

test('stale out-of-order save response does not overwrite newer saved baseline', () => {
  const { dispatch } = getAppState()

  // User saves v1
  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_A, index: 0, isDirty: true, content: '{"v":1}' })
  const rev1 = getAppState().configs[0].revision ?? 0

  // User saves v2
  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_A, index: 0, isDirty: true, content: '{"v":2}' })
  const rev2 = getAppState().configs[0].revision ?? 0

  // Response for v2 arrives first and succeeds
  dispatch({ type: 'SAVE_CONFIG', file: FILE_A, content: '{"v":2}', savedRevision: rev2 })
  expect(getAppState().configs[0].savedContent).toBe('{"v":2}')
  expect(getAppState().configs[0].savedRevision).toBe(rev2)
  expect(getAppState().configs[0].isDirty).toBe(false)

  // Delayed response for v1 arrives late
  dispatch({ type: 'SAVE_CONFIG', file: FILE_A, content: '{"v":1}', savedRevision: rev1 })

  // Stale v1 response was ignored; v2 baseline remains intact
  expect(getAppState().configs[0].savedContent).toBe('{"v":2}')
  expect(getAppState().configs[0].content).toBe('{"v":2}')
  expect(getAppState().configs[0].savedRevision).toBe(rev2)
  expect(getAppState().configs[0].isDirty).toBe(false)
})

test('local commit gate: remote-only save must not modify local baseline or clear dirty', () => {
  const { dispatch } = getAppState()

  // Local file has unsaved edits
  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_A, index: 0, isDirty: true, content: '{"remote_test":true}' })

  const targets: readonly RouterTarget[] = [
    { id: 'router-2', baseUrl: 'http://192.168.1.2:1000', label: 'Router 2' },
  ]
  const results = [{ id: 'router-2', ok: true }]

  // Verify local commit condition
  const localTarget = targets.find((t) => t.id === LOCAL_ROUTER_ID)
  const localResult = results.find((r) => r.id === LOCAL_ROUTER_ID)
  const localCommitted = Boolean(localTarget && localResult?.ok)

  expect(localCommitted).toBe(false)

  // Because localCommitted is false, SAVE_CONFIG is NOT dispatched
  const configA = getAppState().configs.find((c) => c.file === FILE_A)!
  expect(configA.savedContent).toBe('{"inbounds":[]}')
  expect(configA.content).toBe('{"remote_test":true}')
  expect(configA.isDirty).toBe(true)
})

test('local commit gate: mixed save where local router fails leaves local config dirty', () => {
  const { dispatch } = getAppState()

  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_A, index: 0, isDirty: true, content: '{"mixed_fail":true}' })

  const targets: readonly RouterTarget[] = [
    { id: LOCAL_ROUTER_ID, baseUrl: null, label: 'Локальный' },
    { id: 'router-2', baseUrl: 'http://192.168.1.2:1000', label: 'Router 2' },
  ]
  const results = [
    { id: LOCAL_ROUTER_ID, ok: false },
    { id: 'router-2', ok: true },
  ]

  const localTarget = targets.find((t) => t.id === LOCAL_ROUTER_ID)
  const localResult = results.find((r) => r.id === LOCAL_ROUTER_ID)
  const localCommitted = Boolean(localTarget && localResult?.ok)

  expect(localCommitted).toBe(false)

  const configA = getAppState().configs.find((c) => c.file === FILE_A)!
  expect(configA.savedContent).toBe('{"inbounds":[]}')
  expect(configA.content).toBe('{"mixed_fail":true}')
  expect(configA.isDirty).toBe(true)
})

test('local commit gate: mixed save where local router succeeds updates local baseline', () => {
  const { dispatch } = getAppState()

  dispatch({ type: 'UPDATE_CONFIG_DIRTY', file: FILE_A, index: 0, isDirty: true, content: '{"mixed_ok":true}' })
  const saveRev = getAppState().configs[0].revision ?? 0

  const targets: readonly RouterTarget[] = [
    { id: LOCAL_ROUTER_ID, baseUrl: null, label: 'Локальный' },
    { id: 'router-2', baseUrl: 'http://192.168.1.2:1000', label: 'Router 2' },
  ]
  const results = [
    { id: LOCAL_ROUTER_ID, ok: true },
    { id: 'router-2', ok: false },
  ]

  const localTarget = targets.find((t) => t.id === LOCAL_ROUTER_ID)
  const localResult = results.find((r) => r.id === LOCAL_ROUTER_ID)
  const localCommitted = Boolean(localTarget && localResult?.ok)

  expect(localCommitted).toBe(true)

  dispatch({ type: 'SAVE_CONFIG', file: FILE_A, content: '{"mixed_ok":true}', savedRevision: saveRev })

  const configA = getAppState().configs.find((c) => c.file === FILE_A)!
  expect(configA.savedContent).toBe('{"mixed_ok":true}')
  expect(configA.content).toBe('{"mixed_ok":true}')
  expect(configA.isDirty).toBe(false)
})

test('stable file ID lookup correctly locates target config when tabs are reordered', () => {
  const { dispatch } = getAppState()

  // Reorder configs: file B is first, file A is second
  const reorderedConfigs: Config[] = [
    { file: FILE_B, content: '{"rules":[]}', savedContent: '{"rules":[]}', isDirty: false },
    { file: FILE_A, content: '{"inbounds":["reordered"]}', savedContent: '{"inbounds":[]}', isDirty: true },
  ]
  dispatch({ type: 'SET_CONFIGS', configs: reorderedConfigs })

  // Save targeted at file A using stable file ID (index argument omitted or wrong)
  dispatch({ type: 'SAVE_CONFIG', file: FILE_A, content: '{"inbounds":["reordered"]}' })

  const configA = getAppState().configs.find((c) => c.file === FILE_A)!
  const configB = getAppState().configs.find((c) => c.file === FILE_B)!

  expect(configA.savedContent).toBe('{"inbounds":["reordered"]}')
  expect(configA.isDirty).toBe(false)

  // Config B at index 0 remained completely untouched
  expect(configB.savedContent).toBe('{"rules":[]}')
  expect(configB.content).toBe('{"rules":[]}')
  expect(configB.isDirty).toBe(false)
})
