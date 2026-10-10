import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from '@/components/ui/input-group'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { useState } from 'react'
import { isRemoteAuthEnabled, REMOTE_AUTH_UNSUPPORTED, saveRouters } from '../../../lib/multi-routers/actions'
import { DEFAULT_ROUTER_PORT, type RemoteRouter, routerBaseUrl, routerId, routerLabel } from '../../../lib/multi-routers/model'
import { useRoutersStore } from '../../../lib/multi-routers/store'
import { showToast } from '../../../lib/store'

export function AddRouterDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const routers = useRoutersStore((s) => s.routers)
  const [protocol, setProtocol] = useState<'http' | 'https'>('http')
  const [host, setHost] = useState('')
  const [port, setPort] = useState(String(DEFAULT_ROUTER_PORT))
  const [token, setToken] = useState('')
  const [name, setName] = useState('')
  const [saving, setSaving] = useState(false)

  function reset() {
    setProtocol('http')
    setHost('')
    setPort(String(DEFAULT_ROUTER_PORT))
    setToken('')
    setName('')
  }

  async function addRouter() {
    let h = host.trim()
    let proto = protocol
    if (h.startsWith('https://')) {
      proto = 'https'
      h = h.slice(8)
    } else if (h.startsWith('http://')) {
      proto = 'http'
      h = h.slice(7)
    }
    h = h.split('/')[0]
    const p = Number(port) || DEFAULT_ROUTER_PORT
    if (!h) return showToast('Укажите IP или хост', 'error')
    if (p < 1 || p > 65535) return showToast('Неверный порт', 'error')
    const next: RemoteRouter = {
      host: h,
      port: p,
      name: name.trim(),
      protocol: proto,
      token: token.trim() || undefined,
    }
    const id = routerId(next)
    if (routers.some((r) => routerId(r) === id)) return showToast('Роутер уже добавлен', 'error')

    setSaving(true)
    try {
      const authEnabled = await isRemoteAuthEnabled(routerBaseUrl(next.host, next.port, next.protocol), next.token)
      if (authEnabled === true && !next.token) {
        showToast(
          {
            title: 'Авторизация включена',
            body: REMOTE_AUTH_UNSUPPORTED,
          },
          'error'
        )
      }

      await saveRouters([...routers, next])
      onOpenChange(false)
      reset()
      showToast(`Добавлен ${routerLabel(next)}`)
    } catch (e: any) {
      showToast(e.message || 'Ошибка сохранения', 'error')
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(v) => {
        onOpenChange(v)
        if (!v) reset()
      }}
    >
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Добавить роутер</DialogTitle>
          <DialogDescription>
            Укажите адрес панели XKeen UI и токен авторизации (если на удалённом роутере включён вход по паролю).
          </DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-3">
          <div className="flex gap-2">
            <div className="w-28 shrink-0">
              <Select value={protocol} onValueChange={(v) => setProtocol(v === 'https' ? 'https' : 'http')}>
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="Протокол" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="http">http://</SelectItem>
                  <SelectItem value="https">https://</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <InputGroup className="flex-1">
              <InputGroupInput
                value={host}
                onChange={(e) => setHost(e.target.value)}
                placeholder="IP или хост"
                autoFocus
                onKeyDown={(e) => e.key === 'Enter' && addRouter()}
              />
            </InputGroup>
          </div>
          <InputGroup>
            <InputGroupInput
              value={port}
              onChange={(e) => setPort(e.target.value.replace(/\D/g, ''))}
              placeholder="Порт"
              inputMode="numeric"
            />
            <InputGroupAddon align="inline-end">
              <InputGroupText>port</InputGroupText>
            </InputGroupAddon>
          </InputGroup>
          <InputGroup>
            <InputGroupInput
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="Токен авторизации (если включен пароль)"
              type="password"
            />
          </InputGroup>
          <InputGroup>
            <InputGroupInput value={name} onChange={(e) => setName(e.target.value)} placeholder="Имя (необязательно)" />
          </InputGroup>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Отмена
          </Button>
          <Button disabled={saving} onClick={addRouter}>
            Добавить
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
