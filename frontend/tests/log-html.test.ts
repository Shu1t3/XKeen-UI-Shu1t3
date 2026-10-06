import { expect, test } from 'bun:test'
import { escapeLogText, renderLogError } from '../src/lib/log-html'

test('WebSocket errors render as text inside fixed markup', () => {
  for (const payload of ['<img src=x onerror=alert(1)>', '</div><svg onload=alert(1)>', '<script>alert(1)</script>', '" onclick="alert(1)', '&lt;img src=x onerror=alert(1)&gt;']) {
    const html = renderLogError(payload)
    expect(html).toBe(`<div class="log-line" style="color:#ef4444">ERROR: ${escapeLogText(payload)}</div>`)
    expect(html).not.toContain('<img')
    expect(html).not.toContain('<svg')
    expect(html).not.toContain('<script')
    expect(html.match(/<\/div>/g)).toHaveLength(1)
  }
})
test('escape all HTML characters once and retain readable Unicode', () => {
  expect(escapeLogText(`&<>"' кириллица`)).toBe('&amp;&lt;&gt;&quot;&#39; кириллица')
  expect(escapeLogText('&lt;img&gt;')).toBe('&amp;lt;img&amp;gt;')
  expect(escapeLogText('')).toBe('')
})
