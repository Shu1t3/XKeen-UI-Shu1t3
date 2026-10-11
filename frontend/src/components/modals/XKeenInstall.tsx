import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { useEffect, useState } from 'react'
import { apiCall } from '../../lib/api'

type InstallStatus = {
  success: boolean
  installed: boolean
  config_exists: boolean
  partial: boolean
  phase: 'idle' | 'running' | 'complete' | 'failed'
  error?: string
  output: string
}

export function XKeenInstall({ active }: { active: boolean }) {
  const [status, setStatus] = useState<InstallStatus | null>(null)
  const [error, setError] = useState('')
  const [connectionError, setConnectionError] = useState('')
  const [cores, setCores] = useState('xray')
  const [geo, setGeo] = useState(true)
  const [autostart, setAutostart] = useState(false)
  const [token, setToken] = useState('')
  const [submitting, setSubmitting] = useState(false)
  const running = submitting || status?.phase === 'running'

  useEffect(() => {
    if (!active) return
    let disposed = false
    let timer: ReturnType<typeof setTimeout>
    const refresh = async () => {
      try {
        const data = await apiCall<InstallStatus>('GET', 'xkeen/install')
        if (!data.success) throw new Error(data.error || 'Не удалось получить состояние установки')
        if (!disposed) {
          setStatus(data)
          setConnectionError('')
        }
      } catch (e) {
        if (!disposed) setConnectionError(e instanceof Error ? e.message : 'Ошибка соединения')
      } finally {
        if (!disposed) timer = setTimeout(refresh, 2000)
      }
    }
    void refresh()
    return () => {
      disposed = true
      clearTimeout(timer)
    }
  }, [active])

  async function install() {
    setSubmitting(true)
    setError('')
    try {
      const data = await apiCall<{ success: boolean; error?: string }>('POST', 'xkeen/install', {
        cores,
        geo,
        autostart,
        rci_token: token.trim(),
      })
      if (!data.success) throw new Error(data.error || 'Не удалось начать установку')
      setToken('')
      setStatus((s) => s && { ...s, phase: 'running', error: undefined, output: '' })
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Ошибка установки')
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <section className="space-y-3 border-b pb-4">
      <div className="flex items-center justify-between gap-3">
        <span className="text-sm font-medium">Основной сервис XKeen</span>
        <span className="text-muted-foreground text-xs">{status?.installed ? 'Установлен' : status ? 'Не установлен' : 'Проверка…'}</span>
      </div>
      {status && !status.installed && status.phase !== 'running' && !status.partial && (
        <>
          <p className="text-muted-foreground text-xs">
            Первичная установка стабильного релиза jameszeroX/XKeen. Нужны Entware, права root и KeeneticOS 4+. Установщик добавит
            необходимые пакеты. Версии новых ядер выбирает XKeen, включая prerelease.
          </p>
          <Label htmlFor="xkeen-cores">Установить ядра</Label>
          <select
            id="xkeen-cores"
            className="bg-background w-full rounded-md border p-2 text-sm"
            value={cores}
            disabled={running}
            onChange={(e) => setCores(e.target.value)}
          >
            <option value="xray">Xray</option>
            <option value="mihomo">Mihomo</option>
            <option value="all">Xray и Mihomo</option>
            <option value="none">Использовать уже установленные</option>
          </select>
          <label className="flex items-center justify-between gap-2 text-sm">
            Установить геобазы <Switch checked={geo} onCheckedChange={setGeo} disabled={running} />
          </label>
          <label className="flex items-center justify-between gap-2 text-sm">
            Запускать при включении роутера <Switch checked={autostart} onCheckedChange={setAutostart} disabled={running} />
          </label>
          {status.config_exists ? (
            <p className="text-muted-foreground text-xs">
              Настройки берутся из /opt/etc/xkeen/xkeen.json. Для KeeneticOS 5.2+ сохраните в нём RCI-токен перед установкой.
            </p>
          ) : (
            <>
              <Label htmlFor="xkeen-token">RCI-токен для KeeneticOS 5.2+</Label>
              <Input
                id="xkeen-token"
                type="password"
                autoComplete="off"
                value={token}
                disabled={running}
                onChange={(e) => setToken(e.target.value)}
                placeholder="Необязательно для более ранних прошивок"
              />
              <p className="text-muted-foreground text-xs">Создайте токен в разделе «Пользователи и доступ» роутера.</p>
            </>
          )}
          <Button onClick={() => void install()} disabled={running}>
            Установить XKeen
          </Button>
        </>
      )}
      {running && (
        <p className="flex items-center gap-2 text-sm">
          <Spinner className="size-4" /> Установка XKeen… Можно закрыть окно и вернуться позже.
        </p>
      )}
      {status?.partial && !status.installed && status.phase !== 'running' && (
        <p className="text-muted-foreground text-xs">
          Найдена частичная установка. Проверьте журнал и восстановите XKeen через SSH перед повторной установкой.
        </p>
      )}
      {status?.phase === 'complete' && (
        <p className="text-sm text-emerald-500">
          XKeen установлен. Настройте конфигурацию ядра перед запуском. Обновите страницу, чтобы загрузить новые файлы.
        </p>
      )}
      {(error || connectionError || status?.error) && (
        <p role="alert" className="text-destructive text-xs whitespace-pre-wrap">
          {error || connectionError || status?.error}
        </p>
      )}
      {status?.output && (
        <pre className="bg-muted max-h-48 overflow-auto rounded-md p-3 text-xs whitespace-pre-wrap">
          {status.output
            .split(String.fromCharCode(27))
            .map((part) => part.replace(/^\[[0-9;?]*[ -/]*[@-~]/, ''))
            .join('')}
        </pre>
      )}
      <a
        href="https://github.com/jameszeroX/XKeen/wiki/Порядок-установки"
        target="_blank"
        rel="noreferrer"
        className="text-muted-foreground text-xs underline"
      >
        Инструкция XKeen
      </a>
    </section>
  )
}
