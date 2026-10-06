export async function waitForPanelUpdate(
  jobId: string,
  expectedVersion: string,
  request: (url: string, init?: RequestInit) => Promise<Response>,
  timing = { now: Date.now, pause: () => new Promise<void>((resolve) => setTimeout(resolve, 1500)), timeout: 300_000 }
) {
  const deadline = timing.now() + timing.timeout
  while (timing.now() < deadline) {
    await timing.pause()
    let status: Response
    try {
      status = await request(`/api/update/status?job_id=${encodeURIComponent(jobId)}`, { cache: 'no-store' })
    } catch {
      continue
    }
    if (status.ok) {
      const job = (await status.json()) as { job_id?: string; state?: string; error?: string }
      if (job.job_id !== jobId) throw new Error('Получен статус другого обновления')
      if (job.state === 'succeeded') return
      if (job.state === 'failed' || job.state === 'rollback_failed') {
        throw new Error(job.error || 'Обновление не удалось; проверьте журнал восстановления')
      }
    } else if (status.status === 404) {
      // An explicitly selected older panel may not expose the job-status endpoint.
      try {
        const versionRes = await request('/api/version', { cache: 'no-store' })
        if (versionRes.ok) {
          const versions = (await versionRes.json()) as Record<string, { version?: string }>
          if (versions['xkeen-ui']?.version?.replace(/^v/, '') === expectedVersion.replace(/^v/, '')) return
        }
      } catch { /* Retry while restart or recovery is in progress. */ }
    }
  }
  throw new Error('Не получено подтверждение обновления. Проверьте журнал и состояние панели')
}
