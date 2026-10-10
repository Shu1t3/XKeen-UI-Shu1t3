import YAML from 'yaml'

/**
 * Рекурсивно очищает объект от null, undefined, пустых строк и пустых коллекций.
 * Сохраняет логические значения (false), числовые нули (0) и непустые строки/числа.
 */
export function cleanObject<T>(obj: T): T {
  if (obj === null || obj === undefined) return undefined as any
  if (Array.isArray(obj)) {
    const arr = obj.map(cleanObject).filter((v) => v !== undefined)
    return (arr.length > 0 ? arr : undefined) as any
  }
  if (typeof obj === 'object') {
    const res: Record<string, any> = {}
    for (const [k, v] of Object.entries(obj as Record<string, any>)) {
      if (v === null || v === undefined || v === '') continue
      const cleaned = cleanObject(v)
      if (cleaned !== undefined) res[k] = cleaned
    }
    return (Object.keys(res).length > 0 ? res : undefined) as any
  }
  return obj
}

/**
 * Безопасно сериализует единичный прокси Mihomo в формат YAML с сохранением типов скаляров
 * (числовые и булевы строки квотируются, многострочные строки безопасно экранируются).
 */
export function formatMihomoProxyYaml(proxy: Record<string, any>): string {
  const cleaned = cleanObject(proxy)
  if (!cleaned || typeof cleaned !== 'object' || Array.isArray(cleaned)) {
    throw new Error('Некорректная конфигурация прокси')
  }
  const yamlStr = YAML.stringify([cleaned], { indent: 2 }).trimEnd()
  return (
    yamlStr
      .split('\n')
      .map((line) => '  ' + line)
      .join('\n') + '\n'
  )
}

/**
 * Безопасно сериализует proxy-provider Mihomo в формат YAML с сохранением типов.
 */
export function formatMihomoProviderYaml(name: string, provider: Record<string, any>): string {
  const trimmedName = name.trim()
  if (!trimmedName) throw new Error('Имя провайдера не может быть пустым')
  const cleaned = cleanObject(provider)
  if (!cleaned || typeof cleaned !== 'object' || Array.isArray(cleaned)) {
    throw new Error('Некорректная конфигурация провайдера')
  }
  const yamlStr = YAML.stringify({ [trimmedName]: cleaned }, { indent: 2 }).trimEnd()
  return (
    yamlStr
      .split('\n')
      .map((line) => '  ' + line)
      .join('\n') + '\n'
  )
}

/**
 * Валидирует сгенерированный блок прокси Mihomo.
 * Проверяет корректность YAML-синтаксиса, структуру списка, обязательные поля и целостность типов.
 */
export function validateMihomoProxy(content: string, expectedName?: string): Record<string, any>[] {
  if (typeof content !== 'string' || !content.trim()) {
    throw new Error('Конфигурация прокси не может быть пустой')
  }

  const doc = YAML.parseDocument(content)
  if (doc.errors.length > 0) {
    throw new Error(`Сгенерированный прокси не является валидным YAML: ${doc.errors[0].message}`)
  }

  const parsed = doc.toJS()
  if (!Array.isArray(parsed) || parsed.length === 0) {
    throw new Error('Сгенерированный прокси должен быть списком YAML (sequence)')
  }

  for (const p of parsed) {
    if (!p || typeof p !== 'object' || Array.isArray(p)) {
      throw new Error('Элемент прокси должен быть объектом YAML')
    }

    if (typeof p.name !== 'string' || !p.name.trim()) {
      throw new Error('У прокси отсутствует обязательное имя (name)')
    }
    if (expectedName && p.name !== expectedName) {
      throw new Error(`Имя прокси «${p.name}» не совпадает с ожидаемым «${expectedName}»`)
    }

    if (typeof p.type !== 'string' || !p.type.trim()) {
      throw new Error('У прокси отсутствует обязательный тип (type)')
    }

    if (typeof p.server !== 'string' || !p.server.trim()) {
      throw new Error('У прокси отсутствует обязательный адрес сервера (server)')
    }

    if (typeof p.port !== 'number' || !Number.isInteger(p.port) || p.port < 1 || p.port > 65535) {
      throw new Error('У прокси указан некорректный номер порта (port)')
    }

    // Проверка сохранения типов учетных данных и параметров
    if (p.password !== undefined && typeof p.password !== 'string') {
      throw new Error('Пароль прокси (password) должен быть строкой')
    }
    if (p.uuid !== undefined && typeof p.uuid !== 'string') {
      throw new Error('UUID прокси должен быть строкой')
    }
    if (p.token !== undefined && typeof p.token !== 'string') {
      throw new Error('Токен прокси должен быть строкой')
    }

    // Проверка логических флагов
    if (p.udp !== undefined && typeof p.udp !== 'boolean') {
      throw new Error('Параметр udp должен быть логическим значением')
    }
    if (p.tls !== undefined && typeof p.tls !== 'boolean') {
      throw new Error('Параметр tls должен быть логическим значением')
    }
    if (p['skip-cert-verify'] !== undefined && typeof p['skip-cert-verify'] !== 'boolean') {
      throw new Error('Параметр skip-cert-verify должен быть логическим значением')
    }
    if (p.sni !== undefined && typeof p.sni !== 'string') {
      throw new Error('Параметр sni должен быть строкой')
    }
    if (p.servername !== undefined && typeof p.servername !== 'string') {
      throw new Error('Параметр servername должен быть строкой')
    }
    if (p['client-fingerprint'] !== undefined && typeof p['client-fingerprint'] !== 'string') {
      throw new Error('Параметр client-fingerprint должен быть строкой')
    }
    if (p.alpn !== undefined && (!Array.isArray(p.alpn) || p.alpn.some((item: any) => typeof item !== 'string'))) {
      throw new Error('Параметр alpn должен быть списком строк')
    }
  }

  return parsed
}

/**
 * Валидирует сгенерированный блок proxy-provider Mihomo.
 */
export function validateMihomoProvider(content: string, expectedName?: string): { name: string; config: Record<string, any> } {
  if (typeof content !== 'string' || !content.trim()) {
    throw new Error('Конфигурация провайдера не может быть пустой')
  }

  const doc = YAML.parseDocument(content)
  if (doc.errors.length > 0) {
    throw new Error(`Сгенерированный провайдер не является валидным YAML: ${doc.errors[0].message}`)
  }

  const parsed = doc.toJS()
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('Сгенерированный провайдер должен быть объектом YAML')
  }

  const keys = Object.keys(parsed)
  if (keys.length !== 1) {
    throw new Error('Сгенерированный блок провайдера должен содержать ровно одну запись')
  }

  const name = keys[0]
  if (!name.trim()) {
    throw new Error('Имя провайдера не может быть пустым')
  }
  if (expectedName && name !== expectedName) {
    throw new Error(`Имя провайдера «${name}» не совпадает с ожидаемым «${expectedName}»`)
  }

  const config = parsed[name]
  if (!config || typeof config !== 'object' || Array.isArray(config)) {
    throw new Error('Конфигурация провайдера должна быть объектом')
  }

  if (typeof config.type !== 'string' || !config.type.trim()) {
    throw new Error('У провайдера отсутствует обязательный тип (type)')
  }

  if (typeof config.url !== 'string' || !config.url.trim()) {
    throw new Error('У провайдера отсутствует обязательный URL')
  }
  try {
    const parsedUrl = new URL(config.url)
    if (!/^https?:$/i.test(parsedUrl.protocol)) {
      throw new Error('Некорректный протокол')
    }
  } catch (err: any) {
    throw new Error('URL провайдера должен иметь валидный протокол http или https', { cause: err })
  }

  if (typeof config.interval !== 'number' || !Number.isInteger(config.interval) || config.interval <= 0) {
    throw new Error('Интервал обновления (interval) должен быть положительным целым числом')
  }

  if (config['health-check'] !== undefined) {
    const hc = config['health-check']
    if (!hc || typeof hc !== 'object' || Array.isArray(hc)) {
      throw new Error('Параметр health-check должен быть объектом')
    }
    if (typeof hc.enable !== 'boolean') {
      throw new Error('Параметр health-check.enable должен быть логическим значением')
    }
  }

  if (config.header !== undefined) {
    if (!config.header || typeof config.header !== 'object' || Array.isArray(config.header)) {
      throw new Error('Параметр header должен быть объектом')
    }
  }

  return { name, config }
}

/**
 * Валидирует сгенерированный блок outbound для ядра Xray (JSON).
 */
export function validateXrayOutbound(content: string, expectedTag?: string): Record<string, any> {
  if (typeof content !== 'string' || !content.trim()) {
    throw new Error('Конфигурация outbound не может быть пустой')
  }

  let parsed: any
  try {
    parsed = JSON.parse(content)
  } catch (err: any) {
    throw new Error(`Сгенерированный outbound не является валидным JSON: ${err.message}`, { cause: err })
  }

  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('Сгенерированный outbound должен быть объектом JSON')
  }

  if (typeof parsed.tag !== 'string' || !parsed.tag.trim()) {
    throw new Error('У outbound отсутствует обязательный тег (tag)')
  }
  if (expectedTag && parsed.tag !== expectedTag) {
    throw new Error(`Тег outbound «${parsed.tag}» не совпадает с ожидаемым «${expectedTag}»`)
  }

  if (typeof parsed.protocol !== 'string' || !parsed.protocol.trim()) {
    throw new Error('У outbound отсутствует обязательный протокол (protocol)')
  }

  if (!parsed.settings || typeof parsed.settings !== 'object' || Array.isArray(parsed.settings)) {
    throw new Error('У outbound отсутствует конфигурация settings')
  }

  return parsed
}
