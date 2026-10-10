import { describe, expect, test } from 'bun:test'
import YAML from 'yaml'
import { generateConfigForCore } from '../src/lib/outboundParser.js'
import {
  formatMihomoProxyYaml,
  formatMihomoProviderYaml,
  validateMihomoProxy,
  validateMihomoProvider,
  validateXrayOutbound,
} from '../src/lib/configYaml'
import { proxyItemName, providerEntryName, replaceMihomoProxy } from '../src/lib/mihomoReplace'

describe('R31: Safe YAML serialization and configuration validation', () => {
  test('hysteria2: percent-encoded newline in password does not inject skip-cert-verify or change password', () => {
    // Сценарий R31: импорт hysteria2 ссылки с percent-encoded переводом строки в пароле:
    // a%0Askip-cert-verify:%20true
    const uri = 'hysteria2://a%0Askip-cert-verify:%20true@host.example.com:443?sni=host.example.com#hy2-test'
    const res = generateConfigForCore(uri, 'mihomo', '')

    expect(res.type).toBe('proxy')
    expect(res.content).toBeDefined()

    // Проверяем, что полученный блок валиден для Mihomo
    const parsed = validateMihomoProxy(res.content, 'hy2-test')
    expect(parsed.length).toBe(1)

    const proxy = parsed[0]
    expect(proxy.name).toBe('hy2-test')
    expect(proxy.type).toBe('hysteria2')
    expect(proxy.server).toBe('host.example.com')
    expect(proxy.port).toBe(443)

    // Пароль должен быть исходной строкой целиком, включая перевод строки
    expect(typeof proxy.password).toBe('string')
    expect(proxy.password).toBe('a\nskip-cert-verify: true')

    // Внедрённое поле skip-cert-verify должно отсутствовать в объекте прокси!
    expect('skip-cert-verify' in proxy).toBe(false)
    expect(proxy['skip-cert-verify']).toBeUndefined()
  })

  test('numeric passwords retain string type and do not become numbers', () => {
    // Сценарий R31: чисто числовой пароль теряет тип string в старом toYaml
    const numericPasswords = ['123456', '0123', '0', '999999999999']

    for (const numPass of numericPasswords) {
      const uri = `hysteria2://${numPass}@host.example.com:443#hy2-num-${numPass}`
      const res = generateConfigForCore(uri, 'mihomo', '')

      const parsed = validateMihomoProxy(res.content, `hy2-num-${numPass}`)
      const proxy = parsed[0]

      expect(typeof proxy.password).toBe('string')
      expect(proxy.password).toBe(numPass)

      // В самом тексте YAML пароль должен быть закавычен
      expect(res.content).toMatch(new RegExp(`password:\\s*["']${numPass}["']`))
    }
  })

  test('boolean-like and null-like passwords retain string type', () => {
    const specialPasswords = ['true', 'false', 'yes', 'no', 'on', 'off', 'null', '~']

    for (const pw of specialPasswords) {
      const uri = `trojan://${pw}@host.example.com:443#trojan-${pw}`
      const res = generateConfigForCore(uri, 'mihomo', '')

      const parsed = validateMihomoProxy(res.content, `trojan-${pw}`)
      const proxy = parsed[0]

      expect(typeof proxy.password).toBe('string')
      expect(proxy.password).toBe(pw)
    }
  })

  test('passwords with quotes, colons, emojis and symbols are safely serialized', () => {
    const complexPasswords = [
      'pass"with"quotes',
      "pass'with'single",
      'colon:in:password',
      'hash#in#password',
      '  leading-and-trailing  ',
      '🔐секрет-с-эмодзи!@#$%',
    ]

    for (const pw of complexPasswords) {
      const encoded = encodeURIComponent(pw)
      const uri = `hysteria2://${encoded}@host.example.com:443#complex-test`
      const res = generateConfigForCore(uri, 'mihomo', '')

      const parsed = validateMihomoProxy(res.content, 'complex-test')
      expect(parsed[0].password).toBe(pw)
      expect(typeof parsed[0].password).toBe('string')
    }
  })

  test('generated proxy indentation matches Mihomo config.yaml format and integrates with replaceMihomoProxy', () => {
    const uri = 'vless://uuid-1234-5678@example.com:443?security=reality&pbk=publicKey123&sid=shortId123&sni=example.com&fp=chrome#my-vless'
    const res = generateConfigForCore(uri, 'mihomo', '')

    // Начало блока: ровно два пробела перед дефисом
    expect(res.content.startsWith('  - name: my-vless')).toBe(true)

    // proxyItemName должен корректно извлекать имя
    expect(proxyItemName(res.content)).toBe('my-vless')

    // Замена в существующем конфиге должна проходить без синтаксических ошибок
    const configYaml = `proxies:
  - name: old-proxy
    type: ss
    server: 1.1.1.1
    port: 8388
`
    const replaced = replaceMihomoProxy(configYaml, 'old-proxy', res.content, { renameRefs: true })
    expect(replaced.name).toBe('my-vless')
    expect(replaced.text).toContain('name: my-vless')
    expect(replaced.text).toContain('type: vless')

    // Полный результирующий config.yaml должен оставаться валидным YAML
    const doc = YAML.parse(replaced.text)
    expect(doc.proxies.length).toBe(1)
    expect(doc.proxies[0].name).toBe('my-vless')
    expect(doc.proxies[0].type).toBe('vless')
  })

  test('subscription proxy-provider generation produces safe YAML with correct headers and types', () => {
    const subUrl = 'https://example.com/api/subscription?token=secret123'
    const res = generateConfigForCore(subUrl, 'mihomo', '')

    expect(res.type).toBe('proxy-provider')
    const providerValidation = validateMihomoProvider(res.content)
    expect(providerValidation.name).toBe('subscription_1')
    expect(providerValidation.config.type).toBe('http')
    expect(providerValidation.config.url).toBe(subUrl)
    expect(providerValidation.config.interval).toBe(43200)
    expect(providerValidation.config['health-check'].enable).toBe(true)
    expect(providerValidation.config.header['User-Agent']).toEqual(['ClashMeta/1.19.24; mihomo/1.19.24'])
    expect(Array.isArray(providerValidation.config.header['x-hwid'])).toBe(true)

    // providerEntryName должен распознавать сгенерированное имя
    expect(providerEntryName(res.content)).toBe('subscription_1')
  })

  test('formatMihomoProxyYaml directly serializes proxy object and strips null/undefined/empty', () => {
    const raw = {
      name: 'direct-proxy',
      type: 'ss',
      server: '1.2.3.4',
      port: 8388,
      cipher: 'aes-128-gcm',
      password: 'secret-password',
      emptyField: '',
      nullField: null,
      undefinedField: undefined,
    }
    const yaml = formatMihomoProxyYaml(raw)
    expect(yaml).toContain('name: direct-proxy')
    expect(yaml).toContain('password: secret-password')
    expect(yaml).not.toContain('emptyField')
    expect(yaml).not.toContain('nullField')

    const validated = validateMihomoProxy(yaml, 'direct-proxy')
    expect(validated[0].name).toBe('direct-proxy')
  })

  test('formatMihomoProviderYaml quotes numeric and boolean-like names safely', () => {
    const specialNames = ['123', 'true', 'sub: 1', 'sub #1', 'yes']

    for (const name of specialNames) {
      const providerObj = {
        type: 'http',
        url: 'https://example.com/sub',
        interval: 3600,
      }
      const yaml = formatMihomoProviderYaml(name, providerObj)
      const validated = validateMihomoProvider(yaml, name)
      expect(validated.name).toBe(name)
      expect(typeof validated.name).toBe('string')
    }
  })

  test('validateMihomoProxy rejects malformed YAML, invalid types, and corrupted schemas', () => {
    // Пустой контент
    expect(() => validateMihomoProxy('')).toThrow('Конфигурация прокси не может быть пустой')

    // Невалидный YAML
    expect(() => validateMihomoProxy('  - name: test\n    port: [unclosed')).toThrow('не является валидным YAML')

    // Не список
    expect(() => validateMihomoProxy('name: test\ntype: ss')).toThrow('должен быть списком YAML')

    // Отсутствие обязательных полей
    expect(() => validateMihomoProxy('  - name: ""\n    type: ss\n    server: a\n    port: 443')).toThrow('отсутствует обязательное имя')
    expect(() => validateMihomoProxy('  - name: p\n    type: ""\n    server: a\n    port: 443')).toThrow('отсутствует обязательный тип')
    expect(() => validateMihomoProxy('  - name: p\n    type: ss\n    server: ""\n    port: 443')).toThrow('отсутствует обязательный адрес сервера')

    // Некорректный порт
    expect(() => validateMihomoProxy('  - name: p\n    type: ss\n    server: a\n    port: 0')).toThrow('некорректный номер порта')
    expect(() => validateMihomoProxy('  - name: p\n    type: ss\n    server: a\n    port: 70000')).toThrow('некорректный номер порта')
    expect(() => validateMihomoProxy('  - name: p\n    type: ss\n    server: a\n    port: "443"')).toThrow('некорректный номер порта')

    // Пароль не строкового типа (потеря строкового типа)
    expect(() => validateMihomoProxy('  - name: p\n    type: ss\n    server: a\n    port: 443\n    password: 123456')).toThrow(
      'Пароль прокси (password) должен быть строкой'
    )
    expect(() => validateMihomoProxy('  - name: p\n    type: ss\n    server: a\n    port: 443\n    password: true')).toThrow(
      'Пароль прокси (password) должен быть строкой'
    )

    // Несовпадение с ожидаемым именем
    expect(() => validateMihomoProxy('  - name: wrong\n    type: ss\n    server: a\n    port: 443', 'expected')).toThrow(
      'не совпадает с ожидаемым'
    )
  })

  test('validateMihomoProvider rejects invalid schema, multiple keys, and invalid URLs', () => {
    expect(() => validateMihomoProvider('')).toThrow('не может быть пустой')
    expect(() => validateMihomoProvider('  - a\n  - b')).toThrow('должен быть объектом YAML')

    // Несколько ключей в одном блоке
    expect(() => validateMihomoProvider('sub1:\n  type: http\n  url: https://ex.com\n  interval: 1\nsub2:\n  type: http')).toThrow(
      'ровно одну запись'
    )

    // Невалидный URL
    expect(() => validateMihomoProvider('sub1:\n  type: http\n  url: "ftp://ex.com"\n  interval: 100')).toThrow(
      'валидный протокол http или https'
    )
    expect(() => validateMihomoProvider('sub1:\n  type: http\n  url: "not-a-url"\n  interval: 100')).toThrow(
      'валидный протокол http или https'
    )

    // Некорректный интервал
    expect(() => validateMihomoProvider('sub1:\n  type: http\n  url: "https://ex.com"\n  interval: -10')).toThrow(
      'положительным целым числом'
    )
    expect(() => validateMihomoProvider('sub1:\n  type: http\n  url: "https://ex.com"\n  interval: 0')).toThrow(
      'положительным целым числом'
    )
  })

  test('validateXrayOutbound validates JSON structure and rejects invalid payloads', () => {
    const valid = JSON.stringify({
      tag: 'xray-out',
      protocol: 'vless',
      settings: { vnext: [] },
    })
    expect(validateXrayOutbound(valid, 'xray-out').tag).toBe('xray-out')

    // Невалидный JSON
    expect(() => validateXrayOutbound('{ tag: bad')).toThrow('не является валидным JSON')

    // Отсутствие обязательных полей
    expect(() => validateXrayOutbound(JSON.stringify({ protocol: 'vless', settings: {} }))).toThrow('отсутствует обязательный тег')
    expect(() => validateXrayOutbound(JSON.stringify({ tag: 't', settings: {} }))).toThrow('отсутствует обязательный протокол')
    expect(() => validateXrayOutbound(JSON.stringify({ tag: 't', protocol: 'vless' }))).toThrow('отсутствует конфигурация settings')
  })
})
