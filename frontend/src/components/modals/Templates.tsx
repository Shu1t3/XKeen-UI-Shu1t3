import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { IconFileText } from '@tabler/icons-react'
import { useEffect, useRef, useState } from 'react'
import { apiCall, capitalize } from '../../lib/api'
import { getAppState, useAppContext, useModalContext } from '../../lib/store'
import { CONFIG_TEMPLATES } from '../../lib/templates/catalog'
import {
  assertGeoFilesAvailable,
  assertTemplateImportCurrent,
  DEFAULT_TEMPLATE_OPTIONS,
  missingGeoFiles,
  prepareTemplateImport,
  proxyOutboundTags,
  templateTargets,
  type TemplateImport,
} from '../../lib/templates/import'
import { cn } from '../../lib/utils'

interface Props {
  onImport: (preview: TemplateImport) => void
}

export function TemplateModal(props: Props) {
  const { state } = useAppContext()
  const { modals } = useModalContext()
  return <TemplateForm key={`${state.currentCore}:${modals.showTemplateModal}`} {...props} />
}

function TemplateForm({ onImport }: Props) {
  const { state, showToast } = useAppContext({ includeConfigs: true })
  const { modals, dispatch } = useModalContext()
  const templates = CONFIG_TEMPLATES.filter((template) => template.core === state.currentCore)
  const [selectedId, setSelectedId] = useState(templates[0]?.id ?? '')
  const [targetFile, setTargetFile] = useState('')
  const [options, setOptions] = useState(DEFAULT_TEMPLATE_OPTIONS)
  const [preview, setPreview] = useState<TemplateImport | null>(null)
  const [busy, setBusy] = useState(false)
  const [progress, setProgress] = useState('Проверка...')
  const [missing, setMissing] = useState<{ selection: string; files: string[] } | null>(null)
  const request = useRef(0)
  useEffect(
    () => () => {
      request.current++
    },
    []
  )

  const template = templates.find((entry) => entry.id === selectedId)
  const targets = template ? templateTargets(template, state.configs) : []
  const file = targets.some((entry) => entry.file === targetFile) ? targetFile : (targets[0]?.file ?? '')
  const isRouting = template?.section === 'routing'
  let outboundTags: string[] = []
  let outboundError = ''
  if (isRouting) {
    try {
      outboundTags = proxyOutboundTags(state.configs)
    } catch (error) {
      outboundError = error instanceof Error ? error.message : 'Ошибка Outbounds'
    }
  }
  const outboundTag = outboundTags.includes(options.outboundTag) ? options.outboundTag : (outboundTags[0] ?? '')
  const close = () => dispatch({ type: 'SHOW_MODAL', modal: 'showTemplateModal', show: false })

  const selection = JSON.stringify([selectedId, file, options, outboundTag])
  const missingFiles = missing?.selection === selection ? missing.files : []

  async function prepare(installMissing = false) {
    const id = ++request.current
    setBusy(true)
    setProgress('Проверка...')
    setMissing(null)
    try {
      const current = getAppState()
      const result = prepareTemplateImport(selectedId, file, current.currentCore, current.configs, { ...options, outboundTag })
      if (isRouting) {
        const readGeo = async () => {
          const geo = await apiCall<{ success: boolean; site_files?: string[]; ip_files?: string[]; error?: string }>(
            'GET',
            'geo',
            undefined,
            { timeoutMs: 10000 }
          )
          if (!geo.success || !Array.isArray(geo.site_files) || !Array.isArray(geo.ip_files)) {
            throw new Error(geo.error || 'Не удалось проверить геофайлы. Повторите предпросмотр')
          }
          return { site_files: geo.site_files, ip_files: geo.ip_files }
        }
        let geo = await readGeo()
        if (id !== request.current) return
        const required = missingGeoFiles(result.content, geo).map(({ file }) => file)
        if (required.length) {
          setMissing({ selection, files: required })
          if (!installMissing) return
          const latest = getAppState()
          assertTemplateImportCurrent(result, latest.currentCore, latest.configs)
          setProgress('Установка геофайлов...')
          // This is an explicit action: preview alone never writes to the router.
          const installed = await apiCall<{ success: boolean; error?: string }>('POST', 'geo/install', { files: required })
          if (id !== request.current) return
          if (!installed.success) throw new Error(installed.error || 'Не удалось установить геофайлы')
          geo = await readGeo()
          if (id !== request.current) return
          showToast('Геофайлы установлены')
        }
        assertGeoFilesAvailable(result.content, geo)
      }
      if (id !== request.current) return
      const latest = getAppState()
      assertTemplateImportCurrent(result, latest.currentCore, latest.configs)
      setPreview(result)
    } catch (error) {
      if (id === request.current) showToast(error instanceof Error ? error.message : 'Ошибка подготовки шаблона', 'error')
    } finally {
      if (id === request.current) setBusy(false)
    }
  }

  function commit() {
    if (!preview) return
    try {
      onImport(preview)
      close()
    } catch (error) {
      showToast(error instanceof Error ? error.message : 'Ошибка импорта', 'error')
      setPreview(null)
    }
  }

  return (
    <Dialog open={modals.showTemplateModal} onOpenChange={(open) => !open && close()}>
      <DialogContent className={cn('flex max-h-[85vh] flex-col overflow-hidden', preview ? 'max-w-5xl!' : 'max-w-140!')}>
        <DialogHeader className="shrink-0">
          <DialogTitle className="flex items-center gap-2 pr-8 pb-3">
            <IconFileText size={24} className="text-chart-2" />
            {preview ? 'Предпросмотр импорта' : 'Импорт шаблона'}
          </DialogTitle>
          <DialogDescription>
            {preview ? (
              `Файл: ${preview.file}`
            ) : (
              <>
                Встроенные шаблоны для {capitalize(state.currentCore)} <Badge variant="outline">{templates.length}</Badge>
              </>
            )}
          </DialogDescription>
        </DialogHeader>
        {preview ? (
          <>
            <p className="text-muted-foreground text-sm">
              Проверьте заменяемые настройки, включая несохранённые правки. Импорт изменит текст файла в редакторе; для записи на роутер
              используйте «Сохранить» или «Применить».
            </p>
            <div className="grid min-h-0 flex-1 gap-3 overflow-auto md:grid-cols-2">
              {[
                ['До импорта', preview.before],
                ['После импорта', preview.content],
              ].map(([title, content]) => (
                <div key={title} className="min-w-0">
                  <p className="mb-2 text-sm font-medium">{title}</p>
                  <pre className="bg-muted max-h-[45vh] overflow-auto rounded-md border p-3 text-xs">{content || '(пустой файл)'}</pre>
                </div>
              ))}
            </div>
          </>
        ) : (
          <div className="min-h-0 flex-1 overflow-y-auto">
            <div className="space-y-4 pr-2">
              <RadioGroup
                value={selectedId}
                onValueChange={(id) => {
                  setSelectedId(id)
                  setTargetFile('')
                }}
                disabled={busy}
                className="gap-1.5"
              >
                {templates.map((entry) => (
                  <label
                    key={entry.id}
                    className={cn(
                      'flex cursor-pointer items-center gap-3 rounded-lg border px-3 py-3',
                      entry.id === selectedId && 'border-blue-400 bg-blue-500/10'
                    )}
                  >
                    <RadioGroupItem value={entry.id} />
                    <span className="text-sm font-medium">{entry.name}</span>
                  </label>
                ))}
              </RadioGroup>
              {templates.length === 0 && <p className="text-muted-foreground text-sm">Для этого ядра нет шаблонов.</p>}
              <div className="space-y-2">
                <p id="template-target-label" className="text-sm font-medium">
                  Файл назначения
                </p>
                <Select value={file} onValueChange={setTargetFile} disabled={busy || targets.length === 0}>
                  <SelectTrigger aria-labelledby="template-target-label" className="w-full">
                    <SelectValue placeholder="Выберите файл" />
                  </SelectTrigger>
                  <SelectContent>
                    {targets.map((target) => (
                      <SelectItem key={target.file} value={target.file}>
                        {target.file}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                {!file && (
                  <p className="text-sm text-amber-500">
                    {state.currentCore === 'mihomo'
                      ? 'Нужен основной config.yaml.'
                      : 'Нужен файл с выбранным разделом или новый пустой .json. Создайте его в редакторе.'}
                  </p>
                )}
              </div>
              {isRouting && (
                <div className="space-y-2">
                  <p id="template-outbound-label" className="text-sm font-medium">
                    Прокси-подключение
                  </p>
                  <Select
                    value={outboundTag}
                    onValueChange={(value) => setOptions({ ...options, outboundTag: value })}
                    disabled={busy || outboundTags.length === 0}
                  >
                    <SelectTrigger aria-labelledby="template-outbound-label" className="w-full">
                      <SelectValue placeholder="Выберите outbound" />
                    </SelectTrigger>
                    <SelectContent>
                      {outboundTags.map((tag) => (
                        <SelectItem key={tag} value={tag}>
                          {tag}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <p className="text-muted-foreground text-xs">
                    Перед предпросмотром проверяются теги Outbounds и наличие геофайлов на роутере. Категории внутри геофайлов проверит ядро
                    при применении.
                  </p>
                  {missingFiles.length > 0 && (
                    <div className="space-y-2 rounded-md border border-amber-500/40 p-3">
                      <p className="text-sm">Не хватает геофайлов: {missingFiles.join(', ')}.</p>
                      <p className="text-muted-foreground text-xs">
                        Скачаем все недостающие файлы выбранного шаблона и проверим их перед установкой. Работающее ядро не перезапускается.
                        Загрузка может занять несколько минут.
                      </p>
                      <Button variant="outline" disabled={busy} onClick={() => void prepare(true)} className="w-full whitespace-normal">
                        Скачать недостающие файлы
                      </Button>
                    </div>
                  )}
                  {(outboundError || outboundTags.length === 0) && (
                    <p className="text-sm text-amber-500">{outboundError || 'Сначала добавьте прокси-подключение в Outbounds.'}</p>
                  )}
                </div>
              )}
              {state.currentCore === 'mihomo' && (
                <label className="flex cursor-pointer items-start gap-2 text-sm">
                  <Checkbox
                    checked={options.preserveMihomo}
                    disabled={busy}
                    onCheckedChange={(checked) => setOptions({ ...options, preserveMihomo: checked })}
                  />
                  <span>Сохранить подключения, провайдеры, DNS и доступ к API из текущего файла</span>
                </label>
              )}
              {(isRouting || state.currentCore === 'mihomo') && (
                <label className="flex cursor-pointer items-start gap-2 text-sm">
                  <Checkbox
                    checked={options.blockQuic}
                    disabled={busy}
                    onCheckedChange={(checked) => setOptions({ ...options, blockQuic: checked })}
                  />
                  <span>
                    Блокировать QUIC (UDP/443)
                    <span className="text-muted-foreground block text-xs">
                      Может помочь обходу блокировок через TCP, но мешать приложениям, которым нужен UDP/443.
                    </span>
                  </span>
                </label>
              )}
              {state.currentCore === 'mihomo' && (
                <label className="flex cursor-pointer items-start gap-2 text-sm">
                  <Checkbox
                    checked={options.blockAds}
                    disabled={busy}
                    onCheckedChange={(checked) => setOptions({ ...options, blockAds: checked })}
                  />
                  <span>
                    Блокировать рекламу
                    <span className="text-muted-foreground block text-xs">Возможны проблемы с авторизацией и отдельными сервисами.</span>
                  </span>
                </label>
              )}
              {state.currentCore === 'mihomo' && (
                <p className="text-muted-foreground text-xs">
                  Если в текущем файле нет подключений, добавьте сервер после импорта. При отключении сохранения настроек будет заменён весь
                  конфиг.
                </p>
              )}
              {template?.section === 'policy' && (
                <p className="text-muted-foreground text-xs">
                  Для уровня 0 будут использованы стандартные таймауты ядра вместо немедленного закрытия соединения.
                </p>
              )}
            </div>
          </div>
        )}
        <DialogFooter className="shrink-0">
          {preview ? (
            <>
              <Button variant="outline" onClick={() => setPreview(null)}>
                Назад
              </Button>
              <Button onClick={commit}>Вставить в редактор</Button>
            </>
          ) : (
            <Button
              onClick={() => void prepare()}
              disabled={!file || busy || (isRouting && (!outboundTag || !!outboundError))}
              className="w-full"
            >
              {busy ? progress : 'Предпросмотр'}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
