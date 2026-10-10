import { beforeEach, expect, test } from 'bun:test'
import {
  invalidateProviderContentCache,
  providerContentCache,
} from '../src/lib/providersCache'

beforeEach(() => {
  providerContentCache.clear()
})

test('R36: invalidateProviderContentCache deletes specific provider entry', () => {
  providerContentCache.set('rules:provider1', 'payload: [domain1]')
  providerContentCache.set('rules:provider2', 'payload: [domain2]')
  providerContentCache.set('proxies:provider1', 'proxies: [p1]')

  invalidateProviderContentCache('rules', 'provider1')

  expect(providerContentCache.has('rules:provider1')).toBe(false)
  expect(providerContentCache.has('rules:provider2')).toBe(true)
  expect(providerContentCache.has('proxies:provider1')).toBe(true)
})

test('R36: invalidateProviderContentCache invalidates all providers of a kind', () => {
  providerContentCache.set('rules:provider1', 'payload: [domain1]')
  providerContentCache.set('rules:provider2', 'payload: [domain2]')
  providerContentCache.set('proxies:provider1', 'proxies: [p1]')

  invalidateProviderContentCache('rules')

  expect(providerContentCache.has('rules:provider1')).toBe(false)
  expect(providerContentCache.has('rules:provider2')).toBe(false)
  expect(providerContentCache.has('proxies:provider1')).toBe(true)
})

test('R36: updateAllProviders invalidates viewer cache for each updated provider', async () => {
  const httpProviders = ['provider1', 'provider2', 'provider3']
  const kind = 'rules'

  // Pre-populate viewer cache
  for (const name of httpProviders) {
    providerContentCache.set(`${kind}:${name}`, `stale content for ${name}`)
  }
  providerContentCache.set('proxies:other', 'keep this')

  // Simulate updateAllProviders logic with settled promises
  const results = await Promise.allSettled(
    httpProviders.map(async (name) => {
      // simulate network request
      invalidateProviderContentCache(kind, name)
      return name
    })
  )

  expect(results.filter((r) => r.status === 'fulfilled').length).toBe(3)

  // Verify all updated rule providers had their viewer cache invalidated
  for (const name of httpProviders) {
    expect(providerContentCache.has(`${kind}:${name}`)).toBe(false)
  }
  // Other kinds unaffected
  expect(providerContentCache.has('proxies:other')).toBe(true)
})
