import { mihomoBlocked, mihomoExceptRu } from './mihomo'

export type TemplateCore = 'xray' | 'mihomo'
export type XraySection = 'log' | 'inbounds' | 'outbounds' | 'routing' | 'policy'
export interface ConfigTemplate {
  id: string
  name: string
  core: TemplateCore
  section?: XraySection
  content: string
}

const xray = (id: string, name: string, section: XraySection, value: unknown): ConfigTemplate => ({
  id,
  name,
  core: 'xray',
  section,
  content: JSON.stringify({ [section]: value }, null, 2) + '\n',
})
const sniffing = { enabled: true, routeOnly: true, destOverride: ['http', 'tls', 'quic'] }
const tproxy = {
  tag: 'tproxy',
  port: 5000,
  protocol: 'dokodemo-door',
  settings: { network: 'tcp,udp', followRedirect: true },
  streamSettings: { sockopt: { tproxy: 'tproxy' } },
  sniffing,
}
const quic = { port: '443', network: 'udp', outboundTag: 'block' }
const proxy = 'vless-reality' // Replaced with the selected existing outbound before import.

// Snapshot of zxc-rv/assets/config_templates/xray, reviewed 2026-10-11.
// Policy uses core defaults instead of immediate half-close timeouts.
export const CONFIG_TEMPLATES: readonly ConfigTemplate[] = [
  xray('xray-log', 'Log', 'log', { error: '/opt/var/log/xray/error.log', access: 'none', loglevel: 'warning', dnsLog: true }),
  xray('xray-mixed', 'Inbounds (режим Mixed)', 'inbounds', [
    {
      tag: 'redirect',
      port: 5000,
      protocol: 'dokodemo-door',
      settings: { network: 'tcp', followRedirect: true },
      sniffing: { ...sniffing, destOverride: ['http', 'tls'] },
    },
    { ...tproxy, settings: { network: 'udp', followRedirect: true }, sniffing: { ...sniffing, destOverride: ['quic'] } },
  ]),
  xray('xray-tproxy', 'Inbounds (режим TProxy)', 'inbounds', [tproxy]),
  xray('xray-outbounds', 'Outbounds (добавить direct/block)', 'outbounds', [
    { tag: 'direct', protocol: 'freedom' },
    { tag: 'block', protocol: 'blackhole' },
  ]),
  xray('xray-routing-blocked', 'Routing (только заблокированное, zkeen)', 'routing', {
    rules: [
      quic,
      { domain: ['ext:zkeen.dat:domains', 'ext:zkeen.dat:other', 'ext:zkeen.dat:youtube'], outboundTag: proxy },
      {
        ip: [
          'akamai',
          'amazon',
          'cdn77',
          'cloudflare',
          'contabo',
          'digitalocean',
          'discord',
          'fastly',
          'gcore',
          'hetzner',
          'meta',
          'oracle',
          'ovh',
          'scaleway',
          'telegram',
          'vodafone',
          'vultr',
        ].map((name) => `ext:zkeenip.dat:${name}`),
        outboundTag: proxy,
      },
      { network: 'tcp,udp', outboundTag: 'direct' },
    ],
  }),
  xray('xray-routing-except-ru', 'Routing (всё в прокси, кроме RU)', 'routing', {
    rules: [
      quic,
      { protocol: ['bittorrent'], outboundTag: 'direct' },
      { domain: ['ext:geosite_v2fly.dat:category-ru', 'ext:geosite_v2fly.dat:steam'], outboundTag: 'direct' },
      { ip: ['ext:zkeenip.dat:ru'], outboundTag: 'direct' },
      { network: 'tcp,udp', outboundTag: proxy },
    ],
  }),
  xray('xray-policy', 'Policy (стандартные таймауты)', 'policy', { levels: { '0': {} } }),
  { id: 'mihomo-blocked', name: 'Config (только заблокированное, refilter+zkeenip)', core: 'mihomo', content: mihomoBlocked },
  { id: 'mihomo-except-ru', name: 'Config (всё в прокси, кроме RU)', core: 'mihomo', content: mihomoExceptRu },
]

export function getTemplate(id: string): ConfigTemplate {
  const template = CONFIG_TEMPLATES.find((entry) => entry.id === id)
  if (!template) throw new Error('Шаблон не найден')
  return template
}
