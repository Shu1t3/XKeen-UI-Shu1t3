/** Escape untrusted text before surrounding it with fixed journal markup. */
export function escapeLogText(text: string): string {
  const entities: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }
  return text.replace(/[&<>"']/g, (char) => entities[char])
}

export function renderLogError(error: string): string {
  return `<div class="log-line" style="color:#ef4444">ERROR: ${escapeLogText(error)}</div>`
}
