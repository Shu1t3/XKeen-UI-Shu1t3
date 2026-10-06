import { expect, test } from 'bun:test'
import { appendLogLines, MAX_LOG_LINES } from '../src/lib/log-buffer'

// Variable-height scroll container, including the browser's scrollTop clamping.
class Journal {
  nodes: { html: string; height: number; getBoundingClientRect: () => { top: number } }[] = []
  position = 0
  clientHeight = 40
  peakNodes = 0
  get children() { return { item: (index: number) => this.nodes[index] ?? null } }
  get firstChild() { return this.nodes[0] ?? null }
  get scrollHeight() { return Math.max(this.clientHeight, this.nodes.reduce((sum, node) => sum + node.height, 0)) }
  get scrollTop() { return this.position }
  set scrollTop(value: number) { this.position = Math.max(0, Math.min(value, this.scrollHeight - this.clientHeight)) }
  removeChild(node: Journal['nodes'][number]) {
    this.nodes.splice(this.nodes.indexOf(node), 1)
    this.scrollTop = this.position
  }
  insertAdjacentHTML(_position: string, html: string) {
    for (const match of html.matchAll(/<div data-height="(\d+)">.*?<\/div>/g)) {
      const node = {
        html: match[0], height: Number(match[1]),
        getBoundingClientRect: () => ({ top: this.nodes.slice(0, this.nodes.indexOf(node)).reduce((sum, n) => sum + n.height, 0) - this.scrollTop }),
      }
      this.nodes.push(node)
    }
    this.peakNodes = Math.max(this.peakNodes, this.nodes.length)
  }
  asElement() { return this as unknown as HTMLDivElement }
}
const line = (id: number, height = 10) => `<div data-height="${height}">${id}</div>`

test('long stream stays bounded in cache and DOM while reading older logs', () => {
  const el = new Journal()
  let lines: string[] = []
  for (let i = 0; i < 5000; i += 100) {
    lines = appendLogLines(el.asElement(), lines, Array.from({ length: 100 }, (_, j) => line(i + j)), false)
    expect(lines.length).toBeLessThanOrEqual(MAX_LOG_LINES)
    expect(el.nodes.length).toBe(lines.length)
  }
  expect(lines[0]).toBe(line(4000)); expect(lines.at(-1)).toBe(line(4999))
  expect(el.peakNodes).toBe(MAX_LOG_LINES); expect(el.scrollTop).toBe(0)
})

test('retention preserves a surviving viewport with variable-height lines', () => {
  const el = new Journal()
  let lines = Array.from({ length: 1000 }, (_, i) => line(i, i % 3 === 0 ? 30 : 10))
  el.insertAdjacentHTML('beforeend', lines.join('')); el.scrollTop = 4000
  const reading = el.nodes[250], top = reading.getBoundingClientRect().top
  lines = appendLogLines(el.asElement(), lines, Array.from({ length: 100 }, (_, i) => line(1000 + i)), false)
  expect(reading.getBoundingClientRect().top).toBe(top)
  expect(el.scrollTop).toBeLessThan(4000)
  expect(lines[0]).toBe(line(100, 10)); expect(lines).toHaveLength(1000)
})

test('evicted viewport moves to oldest retained line; huge batch is bounded before rendering', () => {
  const el = new Journal()
  let lines = Array.from({ length: 1000 }, (_, i) => line(i))
  el.insertAdjacentHTML('beforeend', lines.join('')); el.scrollTop = 5
  lines = appendLogLines(el.asElement(), lines, [line(1000)], false)
  expect(el.scrollTop).toBe(0)
  el.scrollTop = 7000
  lines = appendLogLines(el.asElement(), lines, Array.from({ length: 200000 }, (_, i) => line(i)), false)
  expect(el.peakNodes).toBe(1000); expect(el.nodes).toHaveLength(1000)
  expect(lines[0]).toBe(line(199000)); expect(el.scrollTop).toBe(0)
})

test('following tails stays at bottom; empty append leaves buffer and viewport alone', () => {
  const el = new Journal()
  const lines = appendLogLines(el.asElement(), [], Array.from({ length: 1100 }, (_, i) => line(i)), true)
  expect(lines).toHaveLength(1000)
  expect(el.scrollTop).toBe(el.scrollHeight - el.clientHeight)
  const position = el.scrollTop
  expect(appendLogLines(el.asElement(), lines, [], false)).toBe(lines)
  expect(el.scrollTop).toBe(position)
})
