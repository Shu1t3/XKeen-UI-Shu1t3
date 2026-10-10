export type ProvidersModalKind = 'rules' | 'proxies'

export const providerContentCache = new Map<string, string>()

export function invalidateProviderContentCache(kind?: ProvidersModalKind, name?: string) {
  if (kind && name) {
    providerContentCache.delete(`${kind}:${name}`)
  } else if (kind) {
    for (const key of Array.from(providerContentCache.keys())) {
      if (key.startsWith(`${kind}:`)) {
        providerContentCache.delete(key)
      }
    }
  } else {
    providerContentCache.clear()
  }
}
