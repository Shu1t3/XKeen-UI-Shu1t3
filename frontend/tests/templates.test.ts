import { expect, test } from 'bun:test'
import { parse as parseJsonc } from 'jsonc-parser'
import { parseDocument } from 'yaml'
import { CONFIG_TEMPLATES, getTemplate } from '../src/lib/templates/catalog'
import {
  assertGeoFilesAvailable,
  assertTemplateImportCurrent,
  DEFAULT_TEMPLATE_OPTIONS,
  missingGeoFiles,
  prepareTemplateImport,
  proxyOutboundTags,
  requiredGeoFiles,
  templateTargets,
} from '../src/lib/templates/import'
import type { Config } from '../src/lib/types'

const config = (file: string, content: string, revision = 0): Config => ({
  file,
  content,
  savedContent: content,
  isDirty: false,
  revision,
  savedRevision: 0,
})
const routeFile = '/opt/etc/xray/configs/05_routing.json'
const outboundFile = '/opt/etc/xray/configs/04_outbounds.json'
const mihomoFile = '/opt/etc/mihomo/config.yaml'
const routes = config(routeFile, '{"routing":{"domainStrategy":"IPOnDemand","rules":[]}}')
const servers = config(
  outboundFile,
  JSON.stringify({
    outbounds: [
      { tag: 'my-server', protocol: 'vless', settings: { id: 'keep-me' } },
      { tag: 'direct', protocol: 'freedom' },
      { tag: 'block', protocol: 'blackhole' },
    ],
  })
)
const options = { ...DEFAULT_TEMPLATE_OPTIONS, outboundTag: 'my-server' }
const yaml = (text: string) => parseDocument(text, { merge: true }).toJS({ maxAliasCount: 1000 })
const prepare = (id: string, file: Config, configs: Config[], opts = options) =>
  prepareTemplateImport(id, file.file, id.startsWith('mihomo') ? 'mihomo' : 'xray', configs, opts)

for (const template of CONFIG_TEMPLATES)
  test(`bundled template parses: ${template.id}`, () => {
    if (template.core === 'xray') return expect(Object.keys(JSON.parse(template.content))).toEqual([template.section!])
    const doc = parseDocument(template.content, { merge: true })
    expect(doc.errors).toEqual([])
    const value = doc.toJS({ maxAliasCount: 1000 })
    const groups = new Set([...value['proxy-groups'].map((g: any) => g.name), 'DIRECT', 'PASS'])
    for (const group of value['proxy-groups']) for (const ref of group.proxies ?? []) expect(groups.has(ref)).toBe(true)
    for (const rule of value.rules)
      for (const match of rule.matchAll(/RULE-SET,([^,()]+)/g)) expect(value['rule-providers'][match[1]]).toBeDefined()
  })

test('targets exclude wrong sections, ancillary files and invalid JSON; empty object is accepted', () => {
  const empty = config('/opt/etc/xray/configs/new.json', '{}')
  const list = [
    routes,
    servers,
    empty,
    config('/opt/etc/xkeen/xkeen.json', '{}'),
    config('/opt/etc/xray/configs/broken.json', '{oops'),
    config('/opt/etc/xkeen/ports.lst', ''),
  ]
  expect(templateTargets(getTemplate('xray-routing-blocked'), list).map((c) => c.file)).toEqual([routeFile, empty.file])
  expect(
    templateTargets(getTemplate('mihomo-blocked'), [config(mihomoFile, ''), config('/opt/etc/mihomo/extra.yaml', '')]).map((c) => c.file)
  ).toEqual([mihomoFile])
})

test('routing replaces proxy tag and preserves domainStrategy, unrelated sections and comments', () => {
  const file = config(routeFile, '// keep this comment\n{"log":{"loglevel":"error"},"routing":{"domainStrategy":"IPOnDemand","rules":[]}}')
  const result = prepare('xray-routing-blocked', file, [file, servers])
  const value = parseJsonc(result.content)
  expect(result.content).toContain('// keep this comment')
  expect(value.log).toEqual({ loglevel: 'error' })
  expect(value.routing.domainStrategy).toBe('IPOnDemand')
  expect(value.routing.rules.some((r: any) => r.outboundTag === 'vless-reality')).toBe(false)
  expect(value.routing.rules.some((r: any) => r.outboundTag === 'my-server')).toBe(true)
  expect(value.routing.rules.some((r: any) => r.network === 'udp' && r.port === '443')).toBe(false)
  const quic = prepare('xray-routing-except-ru', file, [file, servers], { ...options, blockQuic: true })
  expect(parseJsonc(quic.content).routing.rules[0]).toEqual({ port: '443', network: 'udp', outboundTag: 'block' })
})

test('routing rejects absent, duplicate or incompatible outbound tags', () => {
  expect(() => prepare('xray-routing-blocked', routes, [routes])).toThrow('Выберите')
  const missing = config(outboundFile, '{"outbounds":[{"tag":"my-server","protocol":"vless"}]}')
  expect(() => prepare('xray-routing-blocked', routes, [routes, missing])).toThrow('direct')
  const duplicate = config('/opt/etc/xray/configs/extra.json', '{"outbounds":[{"tag":"my-server","protocol":"vless"}]}')
  expect(() => prepare('xray-routing-blocked', routes, [routes, servers, duplicate])).toThrow('ровно один')
  const wrong = config(outboundFile, servers.content.replace('"freedom"', '"blackhole"'))
  expect(() => prepare('xray-routing-blocked', routes, [routes, wrong])).toThrow('freedom')
})

test('Outbounds preserves credentials, adds only missing built-ins and does not duplicate other files', () => {
  const file = config(outboundFile, '{"outbounds":[{"tag":"my-server","protocol":"vless","settings":{"id":"keep-me"}}]}')
  const result = prepare('xray-outbounds', file, [file])
  const value = parseJsonc(result.content).outbounds
  expect(value[0].settings.id).toBe('keep-me')
  expect(value.map((o: any) => o.tag)).toEqual(['my-server', 'direct', 'block'])
  const again = config(outboundFile, result.content)
  expect(parseJsonc(prepare('xray-outbounds', again, [again]).content).outbounds).toHaveLength(3)
  const empty = config(outboundFile, '{}')
  expect(
    parseJsonc(prepare('xray-outbounds', empty, [empty, config('/opt/etc/xray/configs/extra.json', servers.content)]).content).outbounds
  ).toEqual([])
  const wrong = config(outboundFile, '{"outbounds":[{"tag":"direct","protocol":"vless"}]}')
  expect(() => prepare('xray-outbounds', wrong, [wrong])).toThrow('занят')
})

test('preview checks target, dependencies, revisions, core and tampering; tab order is irrelevant', () => {
  const list = [routes, servers]
  const preview = prepare('xray-routing-blocked', routes, list)
  expect(() => assertTemplateImportCurrent(preview, 'xray', [...list].reverse())).not.toThrow()
  for (const changed of [
    [config(routeFile, routes.content + ' '), servers],
    [routes, config(outboundFile, servers.content + ' ')],
    [routes, { ...servers, revision: 1 }],
    [routes],
    [...list, config('/opt/etc/xray/configs/new.json', '{}')],
  ]) {
    expect(() => assertTemplateImportCurrent(preview, 'xray', changed)).toThrow('изменилась')
  }
  expect(() => assertTemplateImportCurrent(preview, 'mihomo', list)).toThrow('Ядро')
  expect(() => assertTemplateImportCurrent({ ...preview, content: '{}' }, 'xray', list)).toThrow('устарел')
})

test('geo prerequisites distinguish IP and domain files', () => {
  const result = prepare('xray-routing-blocked', routes, [routes, servers])
  expect(requiredGeoFiles(result.content)).toEqual([
    { file: 'zkeen.dat', kind: 'domain' },
    { file: 'zkeenip.dat', kind: 'ip' },
  ])
  expect(() => assertGeoFilesAvailable(result.content, { site_files: ['zkeen.dat'], ip_files: ['zkeenip.dat'] })).not.toThrow()
  expect(() => assertGeoFilesAvailable(result.content, { site_files: ['zkeen.dat', 'zkeenip.dat'], ip_files: [] })).toThrow('zkeenip.dat')
})

const oldMihomo = `shared: &resolver [https://dns.example/dns-query]
proxies:
  - { name: my-server, type: socks5, server: server.example, port: 1080, password: keep-me }
proxy-providers:
  custom: { type: http, url: https://provider.example/list, path: ./providers/custom.yaml }
dns:
  enable: true
  listen: 127.0.0.1:1053
  nameserver: *resolver
  nameserver-policy: { 'rule-set:custom-domains': https://dns.example/dns-query }
external-controller: 127.0.0.1:9090
secret: keep-secret
profile: { store-selected: true }
proxy-groups:
  - { name: My custom group, type: select, proxies: [my-server] }
rule-providers:
  custom-domains: { type: inline, behavior: domain, payload: [+.example.com] }
`
for (const id of ['mihomo-blocked', 'mihomo-except-ru'])
  test(`${id}: preserves providers, credentials, DNS aliases and custom groups`, () => {
    const file = config(mihomoFile, oldMihomo)
    const value = yaml(prepare(id, file, [file]).content)
    const old = yaml(oldMihomo)
    for (const key of ['proxies', 'proxy-providers', 'dns', 'secret', 'external-controller', 'profile'])
      expect(value[key]).toEqual(old[key])
    expect(value['proxy-groups'].some((g: any) => g.name === 'My custom group')).toBe(true)
    expect(value['rule-providers']['custom-domains']).toEqual(old['rule-providers']['custom-domains'])
    expect(value['rule-providers']['telegram@ipcidr'].format).toBe('mrs')
    expect(value.rules).not.toContain('RULE-SET,quic@inline,REJECT')
    expect(value.rules).not.toContain('RULE-SET,adlist@domain,REJECT')
    expect(value['rule-providers']['quic@inline']).toBeUndefined()
    expect(value['rule-providers']['adlist@domain']).toBeUndefined()
  })

test('Mihomo blocking options and full replacement are explicit; unapproved defaults stay unchanged', () => {
  const file = config(mihomoFile, oldMihomo)
  const value = yaml(
    prepare('mihomo-blocked', file, [file], { ...options, preserveMihomo: false, blockQuic: true, blockAds: true }).content
  )
  expect(value.dns).toBeUndefined()
  expect(value.proxies).toBeUndefined()
  expect(value.secret).toBeUndefined()
  expect(value.rules).toContain('RULE-SET,quic@inline,REJECT')
  expect(value.rules).toContain('RULE-SET,adlist@domain,REJECT')
  expect(value['external-controller']).toBe('0.0.0.0:9090')
  expect(value['log-level']).toBe('silent')
})

test('invalid YAML and preserved proxy names conflicting with template groups fail without mutation', () => {
  const invalid = config(mihomoFile, 'proxies: [')
  expect(() => prepare('mihomo-blocked', invalid, [invalid])).toThrow('YAML')
  const clash = config(mihomoFile, oldMihomo.replace('name: my-server', 'name: Fallback'))
  expect(() => prepare('mihomo-blocked', clash, [clash])).toThrow('совпадает')
  expect(clash.isDirty).toBe(false)
})

test('policy restores half-close defaults and preserves other levels and settings', () => {
  const file = config(
    '/opt/etc/xray/configs/06_policy.json',
    '{"policy":{"levels":{"0":{"uplinkOnly":0,"downlinkOnly":0,"connIdle":600},"1":{"handshake":10}},"system":{"statsInboundUplink":true}}}'
  )
  const value = parseJsonc(prepare('xray-policy', file, [file]).content).policy
  expect(value.levels['0']).toEqual({ connIdle: 600 })
  expect(value.levels['1']).toEqual({ handshake: 10 })
  expect(value.system).toEqual({ statsInboundUplink: true })
})

test('proxy choices include existing servers only', () => {
  expect(proxyOutboundTags([servers, config('/opt/etc/xkeen/config.json', 'bad')])).toEqual(['my-server'])
})

test('disabled ad blocking retains a provider referenced by preserved DNS', () => {
  const before = oldMihomo.replace('custom-domains', 'adlist@domain')
  const file = config(mihomoFile, before)
  const value = yaml(prepare('mihomo-blocked', file, [file]).content)
  expect(value.dns).toEqual(yaml(before).dns)
  expect(value['rule-providers']['adlist@domain']).toEqual(yaml(before)['rule-providers']['adlist@domain'])
  expect(value.rules).not.toContain('RULE-SET,adlist@domain,REJECT')
})

test('preserving a Unix-only Mihomo API does not enable an additional TCP listener', () => {
  const before = oldMihomo.replace('external-controller: 127.0.0.1:9090', 'external-controller-unix: ./mihomo.sock')
  const file = config(mihomoFile, before)
  const value = yaml(prepare('mihomo-blocked', file, [file]).content)
  expect(value['external-controller-unix']).toBe('./mihomo.sock')
  expect(value['external-controller']).toBeUndefined()
  expect(value.secret).toBe('keep-secret')
})

test('Outbounds refuses duplicate built-in tags instead of pretending to repair them', () => {
  const duplicate = config('/opt/etc/xray/configs/extra.json', '{"outbounds":[{"tag":"direct","protocol":"freedom"}]}')
  expect(() => prepare('xray-outbounds', servers, [servers, duplicate])).toThrow('повторяется')
})

test('missing geodata lists every requirement with its type and excludes installed files', () => {
  const file = prepare('xray-routing-blocked', routes, [routes, servers])
  expect(missingGeoFiles(file.content, { site_files: ['zkeen.dat'], ip_files: [] })).toEqual([{ file: 'zkeenip.dat', kind: 'ip' }])
  expect(missingGeoFiles(file.content, { site_files: [], ip_files: [] })).toEqual([
    { file: 'zkeen.dat', kind: 'domain' },
    { file: 'zkeenip.dat', kind: 'ip' },
  ])
  expect(missingGeoFiles(file.content, { site_files: ['zkeenip.dat'], ip_files: ['zkeen.dat'] })).toHaveLength(2)
  const exceptRu = prepare('xray-routing-except-ru', routes, [routes, servers])
  expect(missingGeoFiles(exceptRu.content, { site_files: [], ip_files: ['zkeenip.dat'] })).toEqual([
    { file: 'geosite_v2fly.dat', kind: 'domain' },
  ])
})
