export const MAX_LOG_LINES = 1000

/** Keep retention independent from whether the reader follows new messages. */
export function appendLogLines(el: HTMLDivElement, current: string[], incoming: string[], follow: boolean): string[] {
  if (incoming.length === 0) return current
  // Bound the batch before creating DOM nodes; a single large message must not
  // temporarily render an unbounded journal or overflow a spread argument list.
  const added = incoming.slice(-MAX_LOG_LINES)
  const removed = Math.max(0, current.length + added.length - MAX_LOG_LINES)
  const anchor = el.children.item(removed)
  const anchorTop = !follow && anchor ? anchor.getBoundingClientRect().top : null

  for (let i = 0; i < removed && el.firstChild; i++) el.removeChild(el.firstChild)
  el.insertAdjacentHTML('beforeend', added.join(''))
  const retained = current.slice(removed).concat(added)

  if (follow) {
    el.scrollTop = el.scrollHeight
  } else if (anchor && anchorTop !== null) {
    // Measure the surviving node, including wrapped/variable-height lines and
    // any scrollTop clamping that occurred while the old nodes were removed.
    el.scrollTop += anchor.getBoundingClientRect().top - anchorTop
  } else {
    // The whole previously displayed batch has fallen outside retention.
    el.scrollTop = 0
  }
  return retained
}
