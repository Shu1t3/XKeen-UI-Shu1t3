import { LOCAL_ROUTER_ID, REMOTE_FETCH_TIMEOUT_MS } from './multi-routers/model'

const RETRY_DELAYS = [500, 1000, 2000, 4000, 8000]
const RETRY_STATUSES = new Set([502, 503, 504])

function extractClashErrorMessage(bodyText: string): string {
  const trimmed = bodyText.trim()
  if (!trimmed) return ''
  try {
    const parsed = JSON.parse(trimmed) as { message?: unknown; error?: unknown }
    const message = typeof parsed.message === 'string' ? parsed.message.trim() : typeof parsed.error === 'string' ? parsed.error.trim() : ''
    return message || trimmed
  } catch {
    return trimmed
  }
}

function apiPrefix(baseUrl?: string | null): string {
  return baseUrl ? `${baseUrl}/api` : '/api'
}

function clashPrefix(baseUrl?: string | null): string {
  return baseUrl ? `${baseUrl}/clash` : '/clash'
}

export async function apiCall<T = unknown>(
  method: string,
  endpoint: string,
  body?: unknown,
  options?: { baseUrl?: string | null; timeoutMs?: number; token?: string | null }
): Promise<T> {
  const isGet = method === 'GET'
  const timeoutMs = options?.timeoutMs ?? (options?.baseUrl ? REMOTE_FETCH_TIMEOUT_MS : undefined)
  const controller = timeoutMs ? new AbortController() : null
  const timer = controller ? setTimeout(() => controller.abort(), timeoutMs) : null

  const headers: Record<string, string> = {}
  if (!isGet) headers['Content-Type'] = 'application/json'
  if (options?.token) {
    headers['Authorization'] = `Bearer ${options.token}`
  }

  try {
    const res = await fetch(`${apiPrefix(options?.baseUrl)}/${endpoint}`, {
      method,
      headers,
      body: !isGet ? JSON.stringify(body) : undefined,
      signal: controller?.signal,
    })
    return (await res.json()) as T
  } finally {
    if (timer) clearTimeout(timer)
  }
}

/** Builds the X-Clash-Port/Secret/Unix headers used to route requests to the active Clash API instance. */
export function buildClashHeaders(port?: string | null, secret?: string | null, unix?: string | null): Record<string, string> {
  const headers: Record<string, string> = {}
  if (!unix && port) headers['X-Clash-Port'] = port
  if (!unix && secret) headers['X-Clash-Secret'] = secret
  if (unix) headers['X-Clash-Unix'] = unix
  return headers
}

export async function clashFetch<T = unknown>(
  port: string,
  path: string,
  options?: {
    method?: string
    secret?: string | null
    body?: unknown
    unix?: string | null
    retry?: boolean
    baseUrl?: string | null
  }
): Promise<T> {
  const { method = 'GET', secret, body, unix, retry = true, baseUrl } = options ?? {}
  const canRetry = retry && method === 'GET'
  const normalizedPath = path.replace(/^\/+/, '')

  const headers: Record<string, string> = buildClashHeaders(port, secret, unix)
  if (body !== undefined) headers['Content-Type'] = 'application/json'

  const reqOptions: RequestInit = {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  }

  const maxAttempts = canRetry ? RETRY_DELAYS.length : 0
  const prefix = clashPrefix(baseUrl)

  for (let attempt = 0; attempt <= maxAttempts; attempt++) {
    let res: Response
    try {
      res = await fetch(`${prefix}/${normalizedPath}`, reqOptions)
    } catch (error) {
      if (attempt === maxAttempts) throw error
      await new Promise((r) => setTimeout(r, RETRY_DELAYS[attempt]))
      continue
    }

    if (!res.ok) {
      if (canRetry && RETRY_STATUSES.has(res.status) && attempt < maxAttempts) {
        await new Promise((r) => setTimeout(r, RETRY_DELAYS[attempt]))
        continue
      }
      const bodyText = await res.text().catch(() => '')
      const details = extractClashErrorMessage(bodyText)
      throw new Error(details || `${res.status} ${res.statusText}`)
    }

    if (res.status === 204 || res.headers.get('content-length') === '0') return {} as T
    return (await res.json()) as T
  }
  throw new Error('Max retries exceeded')
}

export type FanOutResult = { id: string; ok: boolean; error?: string }

/**
 * Run mapping function over items with limited concurrency.
 * @param items Array of items to process
 * @param concurrency Maximum concurrent executions (default 6)
 * @param fn Worker function returning Promise
 */
export async function mapConcurrent<T, R>(
  items: readonly T[],
  concurrency: number,
  fn: (item: T, index: number) => Promise<R>
): Promise<R[]> {
  const limit = Math.max(1, concurrency)
  const results = new Array<R>(items.length)
  let currentIndex = 0

  const workers = Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (currentIndex < items.length) {
      const idx = currentIndex++
      results[idx] = await fn(items[idx], idx)
    }
  })

  await Promise.all(workers)
  return results
}

export const ROUTER_CONCURRENCY_LIMIT = 6

export async function fanOutRouters(
  targetIds: string[],
  task: (id: string, baseUrl: string | null) => Promise<void>,
  getBaseUrl: (id: string) => string | null,
  concurrency = ROUTER_CONCURRENCY_LIMIT
): Promise<FanOutResult[]> {
  if (targetIds.length === 0) throw new Error('Выберите хотя бы один роутер')
  return mapConcurrent(targetIds, concurrency, async (id) => {
    try {
      const baseUrl = getBaseUrl(id)
      if (id !== LOCAL_ROUTER_ID && !baseUrl) throw new Error(`Не задан адрес роутера ${id}`)
      await task(id, baseUrl)
      return { id, ok: true }
    } catch (err) {
      const error = err instanceof Error ? err.message : String(err ?? 'Ошибка')
      return { id, ok: false, error }
    }
  })
}

export function getFileLanguage(filename: string): string {
  if (filename.endsWith('.yaml') || filename.endsWith('.yml')) return 'yaml'
  if (filename.endsWith('.lst')) return 'plaintext'
  return 'json'
}

export function capitalize(str: string) {
  return str ? str[0].toUpperCase() + str.slice(1) : ''
}
