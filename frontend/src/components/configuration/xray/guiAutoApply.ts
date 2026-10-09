import { apiCall } from '../../../lib/api'
import { getAppState, type ShowToastFn } from '../../../lib/store'
import type { AppAction } from '../../../lib/types'
import type { CodeMirrorRef } from '../CodeMirror'

export interface ApplyXrayGuiConfigOptions {
  wrapper: CodeMirrorRef
  file: string
  core?: string
  dispatch: (action: AppAction) => void
  showToast: ShowToastFn
}

/**
 * Saves and applies Xray configuration changes initiated from GUI editors (GuiRouting / GuiLog).
 *
 * Requirements (R18):
 * 1. Check save response before baseline update and restart.
 * 2. On save failure, do NOT touch saved baseline, do NOT restart, preserve dirty flag, show error.
 * 3. On save success, commit baseline to store and editor wrapper with revision tracking.
 * 4. Perform restart, show accurate toast, and query actual service status afterwards.
 *    A failed restart must never result in serviceStatus === 'running'.
 */
export async function applyXrayGuiConfig({
  wrapper,
  file,
  core,
  dispatch,
  showToast,
}: ApplyXrayGuiConfigOptions): Promise<boolean> {
  const content = wrapper.getValue()
  const appState = getAppState()
  const currentConfigIndex = appState.configs.findIndex((c) => c.file === file)
  const currentConfig = currentConfigIndex >= 0 ? appState.configs[currentConfigIndex] : undefined
  const saveRevision = currentConfig?.revision ?? 0

  let saveResult: { success: boolean; error?: string }
  try {
    saveResult = await apiCall<{ success: boolean; error?: string }>('PUT', 'configs', {
      file,
      content,
    })
  } catch (err: any) {
    saveResult = { success: false, error: err?.message || 'Ошибка сети при сохранении' }
  }

  if (!saveResult?.success) {
    dispatch({
      type: 'UPDATE_CONFIG_DIRTY',
      file,
      index: currentConfigIndex >= 0 ? currentConfigIndex : 0,
      isDirty: true,
      content,
    })
    showToast(saveResult?.error || 'Ошибка сохранения конфигурации', 'error')
    return false
  }

  // Baseline commit on successful disk write
  dispatch({
    type: 'SAVE_CONFIG',
    file,
    index: currentConfigIndex >= 0 ? currentConfigIndex : undefined,
    content,
    savedRevision: saveRevision,
  })
  wrapper.setSavedContent(content)

  dispatch({
    type: 'SET_SERVICE_STATUS',
    status: 'pending',
    pendingText: 'Перезапуск...',
  })

  let restartResult: { success: boolean; error?: string }
  try {
    restartResult = await apiCall<{ success: boolean; error?: string }>('POST', 'control', {
      action: 'softRestart',
      core: core || 'xray',
    })
  } catch (err: any) {
    restartResult = { success: false, error: err?.message || 'Ошибка сети при перезапуске' }
  }

  if (restartResult?.success) {
    showToast('Изменения применены', 'success')
  } else {
    showToast(restartResult?.error ? `Ошибка: ${restartResult.error}` : 'Ошибка перезапуска', 'error')
  }

  let actualRunning = Boolean(restartResult?.success)
  try {
    const statusRes = await apiCall<{ success: boolean; running?: boolean }>('GET', 'control')
    if (statusRes?.success && typeof statusRes.running === 'boolean') {
      actualRunning = statusRes.running
    }
  } catch {
    // If status check fails, fallback to restartResult.success
  }

  dispatch({
    type: 'SET_SERVICE_STATUS',
    status: actualRunning ? 'running' : 'stopped',
  })

  return Boolean(saveResult?.success && restartResult?.success && actualRunning)
}
