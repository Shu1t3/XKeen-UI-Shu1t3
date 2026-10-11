import { applyEdits, modify, parse, type ParseError } from 'jsonc-parser'
import { isMap, parseDocument } from 'yaml'
import type { Config } from '../types'
import { getTemplate, type ConfigTemplate } from './catalog'

export interface TemplateOptions {
  outboundTag: string
  blockQuic: boolean
  blockAds: boolean
  preserveMihomo: boolean
}
export const DEFAULT_TEMPLATE_OPTIONS: TemplateOptions = {
  outboundTag: '',
  blockQuic: false,
  blockAds: false,
  preserveMihomo: true,
}
export interface TemplateImport {
  templateId: string
  core: string
  file: string
  before: string
  revision: number
  content: string
  options: TemplateOptions
  // Xray sections depend on the other loaded files as well as the target.
  dependencies: { file: string; content: string; revision: number }[]
}

function jsonObject(content: string): Record<string, any> {
  if (!content.trim()) return {}
  const errors: ParseError[] = []
  const value = parse(content, errors, { allowTrailingComma: true })
  if (errors.length || !value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('Конфигурация должна содержать корректный JSON-объект')
  }
  return value
}

function isCoreFile(config: Config, core: string): boolean {
  return (
    !/(^|\/)xkeen\//.test(config.file) &&
    (core === 'mihomo'
      ? /(^|\/)config\.yaml$/.test(config.file)
      : config.file.endsWith('.json') && !/(^|\/)xkeen(?:-ui)?\.json$/.test(config.file))
  )
}

export function templateTargets(template: ConfigTemplate, configs: readonly Config[]): Config[] {
  return configs.filter((config) => {
    if (!isCoreFile(config, template.core)) return false
    if (template.core === 'mihomo') return true
    try {
      const value = jsonObject(config.content)
      return Object.keys(value).length === 0 || Object.hasOwn(value, template.section!)
    } catch {
      return false
    }
  })
}

function xrayConfigs(configs: readonly Config[]): Config[] {
  return configs.filter((config) => isCoreFile(config, 'xray'))
}

function outbounds(configs: readonly Config[]): Record<string, any>[] {
  return xrayConfigs(configs).flatMap((config) => {
    const value = jsonObject(config.content).outbounds
    if (value !== undefined && !Array.isArray(value)) throw new Error(`Некорректный outbounds: ${config.file}`)
    return value ?? []
  })
}

export function proxyOutboundTags(configs: readonly Config[]): string[] {
  return [
    ...new Set(
      outbounds(configs)
        .filter(
          (item) => item && typeof item.tag === 'string' && item.tag && !['freedom', 'blackhole', 'dns', 'loopback'].includes(item.protocol)
        )
        .map((item) => item.tag)
    ),
  ]
}

function yamlObject(content: string) {
  const doc = parseDocument(content.trim() ? content : '{}', { merge: true })
  if (doc.errors.length || !isMap(doc.contents)) throw new Error('Конфигурация должна содержать корректный YAML-объект')
  return { doc, value: doc.toJS({ maxAliasCount: 1000 }) as Record<string, any> }
}

function prepareMihomo(template: ConfigTemplate, before: string, options: TemplateOptions): string {
  const { doc, value: base } = yamlObject(template.content)
  if (options.preserveMihomo && before.trim()) {
    const { value: old } = yamlObject(before)
    // Materialize aliases before copying: their anchors may live outside these sections.
    const protectedKeys = new Set([
      'proxies',
      'proxy-providers',
      'dns',
      'secret',
      'external-controller',
      'external-controller-unix',
      'external-controller-tls',
      'external-controller-cors',
      'external-ui',
      'external-ui-url',
    ])
    for (const [key, value] of Object.entries(old)) {
      if (protectedKeys.has(key) || !Object.hasOwn(base, key)) doc.set(key, doc.createNode(value))
    }
    if (!Object.hasOwn(old, 'external-controller') && (old['external-controller-unix'] || old['external-controller-tls'])) {
      doc.delete('external-controller')
    }
    for (const key of ['proxies', 'proxy-groups']) {
      const list = old[key]
      if (list !== undefined && (!Array.isArray(list) || list.some((entry: any) => !entry || typeof entry.name !== 'string'))) {
        throw new Error(`Некорректный список ${key}`)
      }
    }
    for (const key of ['proxy-providers', 'rule-providers', 'dns']) {
      const value = old[key]
      if (value !== undefined && (!value || typeof value !== 'object' || Array.isArray(value))) {
        throw new Error(`Некорректный раздел ${key}`)
      }
    }
    const groups = (base['proxy-groups'] ?? []) as { name: string }[]
    const names = new Set(groups.map((group) => group.name))
    if ((old.proxies ?? []).some((entry: { name: string }) => names.has(entry.name))) {
      throw new Error('Имя сохранённого прокси совпадает с группой шаблона. Переименуйте прокси перед импортом')
    }
    doc.set(
      'proxy-groups',
      doc.createNode([...groups, ...(old['proxy-groups'] ?? []).filter((group: { name: string }) => !names.has(group.name))])
    )
    const providers = { ...base['rule-providers'] }
    if (!options.blockQuic) delete providers['quic@inline']
    if (!options.blockAds) delete providers['adlist@domain']
    doc.set('rule-providers', doc.createNode({ ...old['rule-providers'], ...providers }))
  }
  const rules = (base.rules as string[]).filter(
    (rule) =>
      (options.blockQuic || rule !== 'RULE-SET,quic@inline,REJECT') && (options.blockAds || rule !== 'RULE-SET,adlist@domain,REJECT')
  )
  doc.set('rules', doc.createNode(rules))
  // A disabled template blocker may still be used by preserved DNS or sub-rules.
  const references = JSON.stringify(doc.toJS({ maxAliasCount: 1000 }))
  for (const [name, enabled] of [
    ['quic@inline', options.blockQuic],
    ['adlist@domain', options.blockAds],
  ] as const) {
    if (!enabled && !references.includes(`RULE-SET,${name},`) && !references.includes(`rule-set:${name}`)) {
      doc.deleteIn(['rule-providers', name])
    }
  }
  // A second parse catches dangling aliases and malformed output before preview.
  yamlObject(doc.toString())
  return doc.toString()
}

function prepareXray(template: ConfigTemplate, target: Config, configs: readonly Config[], options: TemplateOptions): string {
  const section = template.section!
  const current = jsonObject(target.content)
  let value = jsonObject(template.content)[section]
  if (section === 'policy') {
    const policy = current.policy ?? value
    value = { ...policy, levels: { ...policy.levels, '0': { ...policy.levels?.['0'] } } }
    delete value.levels['0'].uplinkOnly
    delete value.levels['0'].downlinkOnly
  }
  if (section === 'outbounds') {
    const all = outbounds(configs)
    const existing = current.outbounds ?? []
    // Add the built-ins without destroying actual server connections or duplicating tags in other files.
    for (const item of value) {
      const matches = all.filter((entry) => entry?.tag === item.tag)
      if (matches.length > 1) throw new Error(`Тег ${item.tag} повторяется в нескольких outbounds`)
      if (matches.some((entry) => entry.protocol !== item.protocol)) throw new Error(`Тег ${item.tag} занят другим типом outbound`)
    }
    value = [...existing, ...value.filter((item: { tag: string }) => !all.some((entry) => entry?.tag === item.tag))]
  }
  if (section === 'routing') {
    if (!options.outboundTag || !proxyOutboundTags(configs).includes(options.outboundTag)) {
      throw new Error('Выберите существующее прокси-подключение. Сначала добавьте сервер в Outbounds')
    }
    value.rules = value.rules.filter(
      (rule: any) => options.blockQuic || !(rule.network === 'udp' && rule.port === '443' && rule.outboundTag === 'block')
    )
    for (const rule of value.rules) if (rule.outboundTag === 'vless-reality') rule.outboundTag = options.outboundTag
    const all = outbounds(configs)
    const targets = new Set(value.rules.map((rule: any) => rule.outboundTag))
    for (const tag of targets) {
      const matches = all.filter((entry) => entry?.tag === tag)
      if (matches.length !== 1) throw new Error(`Для тега ${tag} нужен ровно один outbound. Проверьте разделы Outbounds`)
      if (tag === 'direct' && matches[0].protocol !== 'freedom') throw new Error('Тег direct должен иметь protocol: freedom')
      if (tag === 'block' && matches[0].protocol !== 'blackhole') throw new Error('Тег block должен иметь protocol: blackhole')
    }
    // Keep the user's domainStrategy, balancers and other routing settings.
    value = { ...current.routing, rules: value.rules }
  }
  const original = target.content.trim() ? target.content : '{}\n'
  return applyEdits(original, modify(original, [section], value, { formattingOptions: { insertSpaces: true, tabSize: 2 } }))
}

export function prepareTemplateImport(
  templateId: string,
  file: string,
  core: string,
  configs: readonly Config[],
  options: TemplateOptions
): TemplateImport {
  const template = getTemplate(templateId)
  if (template.core !== core) throw new Error('Ядро изменилось. Выберите шаблон заново')
  const target = templateTargets(template, configs).find((config) => config.file === file)
  if (!target) throw new Error('Выберите файл с нужным разделом или создайте пустой файл конфигурации')
  return {
    templateId,
    core,
    file,
    before: target.content,
    revision: target.revision ?? 0,
    options: { ...options },
    content: core === 'mihomo' ? prepareMihomo(template, target.content, options) : prepareXray(template, target, configs, options),
    dependencies: (core === 'xray' ? xrayConfigs(configs) : [target]).map((config) => ({
      file: config.file,
      content: config.content,
      revision: config.revision ?? 0,
    })),
  }
}

export function assertTemplateImportCurrent(preview: TemplateImport, core: string, configs: readonly Config[]): void {
  if (core !== preview.core) throw new Error('Ядро изменилось. Откройте предпросмотр заново')
  const current = core === 'xray' ? xrayConfigs(configs) : configs.filter((config) => config.file === preview.file)
  if (
    current.length !== preview.dependencies.length ||
    preview.dependencies.some((snapshot) => {
      const config = current.find((entry) => entry.file === snapshot.file)
      return !config || config.content !== snapshot.content || (config.revision ?? 0) !== snapshot.revision
    })
  )
    throw new Error('Конфигурация изменилась после предпросмотра. Откройте его заново')
  const prepared = prepareTemplateImport(preview.templateId, preview.file, core, configs, preview.options)
  if (prepared.content !== preview.content || prepared.before !== preview.before) throw new Error('Предпросмотр устарел')
}

export function requiredGeoFiles(content: string): { file: string; kind: 'domain' | 'ip' }[] {
  const rules = jsonObject(content).routing?.rules ?? []
  const entries = new Map<string, { file: string; kind: 'domain' | 'ip' }>()
  for (const rule of rules)
    for (const kind of ['domain', 'ip'] as const) {
      for (const value of rule[kind] ?? []) {
        const match = typeof value === 'string' && /^ext:([^:]+):/.exec(value)
        if (match) entries.set(`${kind}:${match[1]}`, { file: match[1], kind })
      }
    }
  return [...entries.values()]
}

export function assertGeoFilesAvailable(content: string, geo: { site_files: string[]; ip_files: string[] }): void {
  const missing = requiredGeoFiles(content).filter(({ file, kind }) => !(kind === 'domain' ? geo.site_files : geo.ip_files).includes(file))
  if (missing.length)
    throw new Error(`Не найдены геофайлы нужного типа: ${missing.map(({ file }) => file).join(', ')}. Установите их перед импортом`)
}
