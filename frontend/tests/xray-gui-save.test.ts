import { afterEach, beforeEach, expect, test } from 'bun:test'
import { applyXrayGuiConfig } from '../src/components/configuration/xray/guiAutoApply'
import type { CodeMirrorRef } from '../src/components/configuration/CodeMirror'
import { getAppState, type ShowToastFn } from '../src/lib/store'
import type { Config } from '../src/lib/types'

const FILE_ROUTES = '/opt/etc/xray/routes.json'

function createMockWrapper(currentValue: string, baselineSavedValue: string = currentValue): {
  wrapper: CodeMirrorRef
  getSaved: () => string
  setContent: (c: string) => void
} {
  let val = currentValue
  let saved = baselineSavedValue
  const wrapper: CodeMirrorRef = {
    getValue: () => val,
    setValue: (v: string) => {
      val = v
      saved = v
    },
    setSavedContent: (c: string) => {
      saved = c
    },
    setLanguage: () => {},
    validate: () => {},
    format: async () => {},
    layout: () => {},
    focus: () => {},
    isValid: () => true,
    saveViewState: () => null,
    restoreViewState: () => {},
    replaceAll: (t: string) => {
      val = t
    },
    replaceRange: () => {},
    getLineCount: () => 1,
    offsetToLineColumn: () => ({ lineNumber: 1, column: 1 }),
    revealLine: () => {},
  }
  return {
    wrapper,
    getSaved: () => saved,
    setContent: (c: string) => {
      val = c
    },
  }
}

const originalFetch = globalThis.fetch
let fetchCalls: Array<{ url: string; method: string; body?: any }> = []
let toasts: Array<{ message: string | { title: string; body: string }; type?: 'success' | 'error' }> = []

beforeEach(() => {
  fetchCalls = []
  toasts = []
  const initialConfigs: Config[] = [
    {
      file: FILE_ROUTES,
      content: '{"routing":{"rules":[]}}',
      savedContent: '{"routing":{"rules":[]}}',
      isDirty: false,
      revision: 0,
      savedRevision: 0,
    },
  ]
  getAppState().dispatch({ type: 'SET_CONFIGS', configs: initialConfigs })
  getAppState().dispatch({ type: 'SET_SERVICE_STATUS', status: 'running' })
})

afterEach(() => {
  globalThis.fetch = originalFetch
})

const mockToast: ShowToastFn = (message, type) => {
  toasts.push({ message, type })
}

test('autoApply save failure: disk error preserves dirty state, does NOT update baseline, and aborts restart', async () => {
  const initialSaved = '{"routing":{"rules":[]}}'
  const newContent = '{"routing":{"rules":[{"outboundTag":"direct"}]}}'

  // User edits via GUI: CodeMirror replaceAll triggers UPDATE_CONFIG_DIRTY
  getAppState().dispatch({
    type: 'UPDATE_CONFIG_DIRTY',
    file: FILE_ROUTES,
    index: 0,
    isDirty: true,
    content: newContent,
  })

  const { wrapper, getSaved } = createMockWrapper(newContent, initialSaved)

  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    fetchCalls.push({ url, method, body })

    if (url.includes('/api/configs') && method === 'PUT') {
      return Response.json({ success: false, error: 'Диск переполнен: No space left on device' })
    }
    return Response.json({ success: true })
  }) as typeof fetch

  const success = await applyXrayGuiConfig({
    wrapper,
    file: FILE_ROUTES,
    core: 'xray',
    dispatch: getAppState().dispatch,
    showToast: mockToast,
  })

  expect(success).toBe(false)

  // Verify PUT /api/configs was executed
  const putConfigCall = fetchCalls.find((c) => c.url.includes('/api/configs') && c.method === 'PUT')
  expect(putConfigCall).toBeDefined()
  expect(putConfigCall?.body?.file).toBe(FILE_ROUTES)

  // Verify softRestart was NEVER attempted because save failed
  const restartCall = fetchCalls.find((c) => c.url.includes('/api/control') && c.method === 'POST')
  expect(restartCall).toBeUndefined()

  // Verify editor baseline was NOT corrupted
  expect(getSaved()).toBe(initialSaved)

  // Verify config in store remains dirty and savedContent is untouched
  const config = getAppState().configs.find((c) => c.file === FILE_ROUTES)!
  expect(config.isDirty).toBe(true)
  expect(config.savedContent).toBe(initialSaved)
  expect(config.content).toBe(newContent)

  // Verify error toast was shown, and success toast was NOT shown
  expect(toasts.some((t) => t.type === 'error' && String(t.message).includes('Диск переполнен'))).toBe(true)
  expect(toasts.some((t) => t.message === 'Изменения применены')).toBe(false)

  // Service status remains running (not changed to pending or stopped)
  expect(getAppState().serviceStatus).toBe('running')
})

test('autoApply save failure: network exception preserves dirty state and blocks restart', async () => {
  const initialSaved = '{"routing":{"rules":[]}}'
  const newContent = '{"routing":{"rules":[{"outboundTag":"proxy"}]}}'

  getAppState().dispatch({
    type: 'UPDATE_CONFIG_DIRTY',
    file: FILE_ROUTES,
    index: 0,
    isDirty: true,
    content: newContent,
  })

  const { wrapper, getSaved } = createMockWrapper(newContent, initialSaved)

  globalThis.fetch = (async () => {
    throw new Error('Connection reset by peer')
  }) as typeof fetch

  const success = await applyXrayGuiConfig({
    wrapper,
    file: FILE_ROUTES,
    core: 'xray',
    dispatch: getAppState().dispatch,
    showToast: mockToast,
  })

  expect(success).toBe(false)
  expect(getSaved()).toBe(initialSaved)

  const config = getAppState().configs.find((c) => c.file === FILE_ROUTES)!
  expect(config.isDirty).toBe(true)
  expect(config.savedContent).toBe(initialSaved)
  expect(toasts.some((t) => t.type === 'error' && String(t.message).includes('Connection reset'))).toBe(true)
  expect(toasts.some((t) => t.message === 'Изменения применены')).toBe(false)
})

test('autoApply success: updates baseline, triggers softRestart, checks actual status, and reports success', async () => {
  const initialSaved = '{"routing":{"rules":[]}}'
  const newContent = '{"routing":{"rules":[{"outboundTag":"direct"}]}}'

  getAppState().dispatch({
    type: 'UPDATE_CONFIG_DIRTY',
    file: FILE_ROUTES,
    index: 0,
    isDirty: true,
    content: newContent,
  })

  const { wrapper, getSaved } = createMockWrapper(newContent, initialSaved)

  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    fetchCalls.push({ url, method, body })

    if (url.includes('/api/configs') && method === 'PUT') {
      return Response.json({ success: true })
    }
    if (url.includes('/api/control') && method === 'POST') {
      return Response.json({ success: true })
    }
    if (url.includes('/api/control') && method === 'GET') {
      return Response.json({ success: true, running: true, currentCore: 'xray' })
    }
    return Response.json({ success: true })
  }) as typeof fetch

  const success = await applyXrayGuiConfig({
    wrapper,
    file: FILE_ROUTES,
    core: 'xray',
    dispatch: getAppState().dispatch,
    showToast: mockToast,
  })

  expect(success).toBe(true)

  // Verify all 3 sequential calls occurred
  expect(fetchCalls).toHaveLength(3)
  expect(fetchCalls[0].method).toBe('PUT')
  expect(fetchCalls[0].url).toContain('/api/configs')
  expect(fetchCalls[1].method).toBe('POST')
  expect(fetchCalls[1].url).toContain('/api/control')
  expect(fetchCalls[1].body).toEqual({ action: 'softRestart', core: 'xray' })
  expect(fetchCalls[2].method).toBe('GET')
  expect(fetchCalls[2].url).toContain('/api/control')

  // Verify editor baseline was updated
  expect(getSaved()).toBe(newContent)

  // Verify store state
  const config = getAppState().configs.find((c) => c.file === FILE_ROUTES)!
  expect(config.savedContent).toBe(newContent)
  expect(config.isDirty).toBe(false)
  expect(getAppState().serviceStatus).toBe('running')

  // Verify success toast
  expect(toasts.some((t) => t.type === 'success' && t.message === 'Изменения применены')).toBe(true)
})

test('autoApply restart failure: baseline is saved, but serviceStatus is stopped, NEVER running', async () => {
  const initialSaved = '{"routing":{"rules":[]}}'
  const newContent = '{"routing":{"rules":[{"outboundTag":"direct"}]}}'

  getAppState().dispatch({
    type: 'UPDATE_CONFIG_DIRTY',
    file: FILE_ROUTES,
    index: 0,
    isDirty: true,
    content: newContent,
  })

  const { wrapper, getSaved } = createMockWrapper(newContent, initialSaved)

  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'
    const body = init?.body ? JSON.parse(String(init.body)) : undefined
    fetchCalls.push({ url, method, body })

    if (url.includes('/api/configs') && method === 'PUT') {
      return Response.json({ success: true })
    }
    if (url.includes('/api/control') && method === 'POST') {
      return Response.json({ success: false, error: 'Xray process terminated unexpectedly with exit code 1' })
    }
    if (url.includes('/api/control') && method === 'GET') {
      // Process is actually dead
      return Response.json({ success: true, running: false, currentCore: 'xray' })
    }
    return Response.json({ success: true })
  }) as typeof fetch

  const success = await applyXrayGuiConfig({
    wrapper,
    file: FILE_ROUTES,
    core: 'xray',
    dispatch: getAppState().dispatch,
    showToast: mockToast,
  })

  expect(success).toBe(false)

  // File was written to disk, so saved baseline in store & editor is updated
  expect(getSaved()).toBe(newContent)
  const config = getAppState().configs.find((c) => c.file === FILE_ROUTES)!
  expect(config.savedContent).toBe(newContent)

  // Crucial check: serviceStatus must be stopped, NOT running!
  expect(getAppState().serviceStatus).toBe('stopped')

  // Error toast was displayed
  expect(toasts.some((t) => t.type === 'error' && String(t.message).includes('exit code 1'))).toBe(true)
  expect(toasts.some((t) => t.message === 'Изменения применены')).toBe(false)
})

test('autoApply restart failure with GET control error: falls back to stopped, NEVER running', async () => {
  const initialSaved = '{"routing":{"rules":[]}}'
  const newContent = '{"routing":{"rules":[{"outboundTag":"direct"}]}}'

  getAppState().dispatch({
    type: 'UPDATE_CONFIG_DIRTY',
    file: FILE_ROUTES,
    index: 0,
    isDirty: true,
    content: newContent,
  })

  const { wrapper } = createMockWrapper(newContent, initialSaved)

  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'

    if (url.includes('/api/configs') && method === 'PUT') {
      return Response.json({ success: true })
    }
    if (url.includes('/api/control') && method === 'POST') {
      return Response.json({ success: false, error: 'Failed to restart' })
    }
    if (url.includes('/api/control') && method === 'GET') {
      throw new Error('Connection refused')
    }
    return Response.json({ success: true })
  }) as typeof fetch

  const success = await applyXrayGuiConfig({
    wrapper,
    file: FILE_ROUTES,
    core: 'xray',
    dispatch: getAppState().dispatch,
    showToast: mockToast,
  })

  expect(success).toBe(false)
  // Fallback must set status to stopped when restart failed, NEVER running!
  expect(getAppState().serviceStatus).toBe('stopped')
})

test('autoApply concurrent edits: typing newer changes while save is in flight preserves newer edits', async () => {
  const contentV1 = '{"routing":{"rules":["rule_v1"]}}'
  const contentV2 = '{"routing":{"rules":["rule_v1","rule_v2_in_flight"]}}'

  // User edits to V1
  getAppState().dispatch({
    type: 'UPDATE_CONFIG_DIRTY',
    file: FILE_ROUTES,
    index: 0,
    isDirty: true,
    content: contentV1,
  })

  const { wrapper } = createMockWrapper(contentV1)

  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'

    if (url.includes('/api/configs') && method === 'PUT') {
      // Simulate user continuing to edit while PUT request is in flight
      getAppState().dispatch({
        type: 'UPDATE_CONFIG_DIRTY',
        file: FILE_ROUTES,
        index: 0,
        isDirty: true,
        content: contentV2,
      })
      return Response.json({ success: true })
    }
    if (url.includes('/api/control') && method === 'POST') {
      return Response.json({ success: true })
    }
    if (url.includes('/api/control') && method === 'GET') {
      return Response.json({ success: true, running: true, currentCore: 'xray' })
    }
    return Response.json({ success: true })
  }) as typeof fetch

  const success = await applyXrayGuiConfig({
    wrapper,
    file: FILE_ROUTES,
    core: 'xray',
    dispatch: getAppState().dispatch,
    showToast: mockToast,
  })

  expect(success).toBe(true)

  const config = getAppState().configs.find((c) => c.file === FILE_ROUTES)!
  // Baseline was committed to disk as V1
  expect(config.savedContent).toBe(contentV1)
  // Current in-memory content retains the user's latest in-flight edits (V2)
  expect(config.content).toBe(contentV2)
  // Config stays dirty because V2 has not yet been saved to disk
  expect(config.isDirty).toBe(true)
})
