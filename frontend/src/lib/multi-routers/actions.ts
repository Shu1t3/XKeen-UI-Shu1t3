import { apiCall, fanOutRouters, type FanOutResult } from '../api'
import { LOCAL_ROUTER_ID, type RemoteRouter, isRouterSelectable, routerId, routerLabel } from './model'
import { getBaseUrlForId, getTokenForId, useRoutersStore } from './store'

export const REMOTE_AUTH_UNSUPPORTED =
  'Удалённая панель защищена паролем. Укажите токен доступа в настройках роутера.'

export async function persistRouters(routers: RemoteRouter[]): Promise<{ success: boolean; error?: string }> {
  return apiCall<{ success: boolean; error?: string }>('PATCH', 'settings', { plugins: { routers } })
}

export async function saveRouters(routers: RemoteRouter[]): Promise<void> {
  const result = await persistRouters(routers)
  if (!result.success) throw new Error(result.error || 'Не удалось сохранить список роутеров')
  useRoutersStore.getState().setRouters(routers)
}

export function applyRoutersFromSettings(routers: RemoteRouter[]): RemoteRouter[] {
  useRoutersStore.getState().setRouters(routers)
  return routers
}

/** `true` / `false` if reachable; `null` if probe failed. */
export async function isRemoteAuthEnabled(baseUrl: string | null, token?: string | null): Promise<boolean | null> {
  if (!baseUrl) return false
  try {
    const data = await apiCall<{ enabled?: boolean }>('GET', 'auth/login', undefined, {
      baseUrl,
      token,
      timeoutMs: 5000,
    })
    return !!data?.enabled
  } catch {
    return null
  }
}

export type RouterTarget = Readonly<{ id: string; baseUrl: string | null; token?: string | null; label: string }>

export function snapshotMassTargets(multiRouter: boolean): readonly RouterTarget[] {
  const ids = multiRouter ? [...useRoutersStore.getState().applyTargets] : [LOCAL_ROUTER_ID]
  if (ids.length === 0) throw new Error('Выберите хотя бы один роутер')
  return Object.freeze(
    ids.map((id) =>
      Object.freeze({
        id,
        baseUrl: getBaseUrlForId(id),
        token: getTokenForId(id),
        label: targetLabel(id),
      })
    )
  )
}

export async function filterAuthBlockedTargets(
  targets: readonly RouterTarget[]
): Promise<{ allowed: readonly RouterTarget[]; blocked: readonly RouterTarget[] }> {
  const blocked = await Promise.all(
    targets.map(async (target) => {
      if (target.id === LOCAL_ROUTER_ID) return false
      try {
        const data = await apiCall<{ enabled?: boolean; authenticated?: boolean }>('GET', 'auth/login', undefined, {
          baseUrl: target.baseUrl,
          token: target.token,
          timeoutMs: 5000,
        })
        return data?.enabled === true && !data?.authenticated
      } catch {
        return false
      }
    })
  )
  return {
    allowed: Object.freeze(targets.filter((_, index) => !blocked[index])),
    blocked: Object.freeze(targets.filter((_, index) => blocked[index])),
  }
}

export async function pingRouterOnline(id: string): Promise<boolean> {
  try {
    const data = await apiCall<{ success?: boolean }>('GET', 'version', undefined, {
      baseUrl: getBaseUrlForId(id),
      token: getTokenForId(id),
      timeoutMs: 5000,
    })
    return !!data?.success
  } catch {
    return false
  }
}

export async function refreshAllOnline(): Promise<void> {
  const { routers, setOnline, setAuth } = useRoutersStore.getState()
  const ids = [LOCAL_ROUTER_ID, ...routers.map(routerId)]
  await Promise.all(
    ids.map(async (id) => {
      const online = await pingRouterOnline(id)
      if (id === LOCAL_ROUTER_ID || online) {
        setOnline(id, online)
        setAuth(id, false)
        return
      }
      // The router may have been removed while its online probe was pending.
      let authEnabled: boolean | null
      try {
        authEnabled = await isRemoteAuthEnabled(getBaseUrlForId(id), getTokenForId(id))
      } catch {
        return
      }
      setOnline(id, false)
      setAuth(id, authEnabled === true)
    })
  )
  const state = useRoutersStore.getState()
  const next = state.applyTargets.filter((id) => isRouterSelectable(id, state.online, state.auth, state.routers))
  if (next.length !== state.applyTargets.length) state.setApplyTargets(next)
}

export function targetLabel(id: string): string {
  if (id === LOCAL_ROUTER_ID) return 'Этот роутер'
  const router = useRoutersStore.getState().routers.find((r) => routerId(r) === id)
  return router ? routerLabel(router) : id
}

export async function runMassTask(
  targets: readonly RouterTarget[],
  task: (id: string, baseUrl: string | null, target?: RouterTarget) => Promise<void>
): Promise<FanOutResult[]> {
  const { resetCommandStatuses, setCommandStatus, getBaseUrlForId: getBase } = useRoutersStore.getState()
  if (targets.length === 0) throw new Error('Выберите хотя бы один роутер')
  const targetIds = targets.map((target) => target.id)
  resetCommandStatuses()
  for (const id of targetIds) setCommandStatus(id, { status: 'pending' })

  const results = await fanOutRouters(
    targetIds,
    async (id, baseUrl) => {
      const target = targets.find((t) => t.id === id)
      await task(id, baseUrl, target)
    },
    (id) => {
      const target = targets.find((target) => target.id === id)!
      if (getBase(id) !== target.baseUrl) throw new Error(`Адрес роутера ${target.label} изменился; повторите выбор`)
      return target.baseUrl
    }
  )

  for (const result of results) {
    setCommandStatus(result.id, {
      status: result.ok ? 'success' : 'error',
      message: result.error,
    })
  }
  return results
}

export function summarizeFanOut(results: FanOutResult[]): { ok: number; fail: number; body: string } {
  const ok = results.filter((r) => r.ok).length
  const fail = results.length - ok
  if (results.length <= 1) {
    return { ok, fail, body: fail ? results[0]?.error || 'ошибка' : '' }
  }
  const lines = results.map((r) => `${targetLabel(r.id)}: ${r.ok ? 'OK' : r.error || 'ошибка'}`)
  return { ok, fail, body: lines.join('; ') }
}
