import { expect, test } from 'bun:test'
import { waitForPanelUpdate } from '../src/lib/update-job'

function clock() {
  let now = 0
  return { now: () => now, pause: async () => { now += 1 }, timeout: 5 }
}
function mockRequest(responses: Array<Response | Error>) {
  return (async () => {
    const response = responses.shift()
    if (response instanceof Error) throw response
    if (!response) throw new Error('connection unavailable')
    return response
  }) as typeof fetch
}
const status = (state: string, error = '', id = 'job') => Response.json({ job_id: id, state, error })

test('pending and a restart disconnect do not report success before supervisor confirmation', async () => {
  const responses = [status('pending'), new Error('disconnected'), status('pending'), status('succeeded')]
  await waitForPanelUpdate('job', '1.0.0', mockRequest(responses), clock())
  expect(responses).toHaveLength(0)
})

test('failed recovery and completed rollback both display the supervisor error', async () => {
  for (const state of ['failed', 'rollback_failed']) {
    await expect(waitForPanelUpdate('job', '1.0.0', mockRequest([status(state, 'recovery result')]), clock())).rejects.toThrow('recovery result')
  }
})

test('a stale job result cannot confirm the current update', async () => {
  await expect(waitForPanelUpdate('job', '1.0.0', mockRequest([status('succeeded', '', 'another-job')]), clock())).rejects.toThrow('другого обновления')
})

test('older panels without a status route must prove the expected running version', async () => {
  await waitForPanelUpdate('job', 'v1.0.0', mockRequest([
    new Response('', { status: 404 }), Response.json({ 'xkeen-ui': { version: '1.0.0' } })
  ]), clock())
})

test('old running version and a missing final result never produce installed confirmation', async () => {
  const request = (async (url: string | URL | Request) => url === '/api/version'
    ? Response.json({ 'xkeen-ui': { version: '0.9.0' } })
    : new Response('', { status: 404 })) as typeof fetch
  await expect(waitForPanelUpdate('job', '1.0.0', request, clock())).rejects.toThrow('Не получено подтверждение')
})
