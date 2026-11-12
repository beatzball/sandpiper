import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { Sandpiper, SandpiperError, ExecError, StreamError } from '../dist/index.js';

const id = 'm_0123456789abcdef';
const exec = 'e_0123456789abcdef';
const info = { id, state: 'running', reason: null, template: 'node', createdAt: 1, expiresAt: 10000, idleMs: 1000 };
const event = (type, seq, data = {}) => ({ machine: id, seq, time: 1, type, ...data });
const started = event('exec.started', 2, { exec, cmd: ['echo', 'hi'] });
const exited = (data = {}) => event('exec.exited', 5, { exec, code: 0, stdoutTruncated: false, stderrTruncated: false, ...data });
const destroyed = event('machine.destroyed', 6, { reason: 'requested' });
const line = value => `${JSON.stringify(value)}\n`;
const stream = (response, events) => {
  response.setHeader('Content-Type', 'application/x-ndjson');
  response.end(events.map(line).join(''));
};
async function json(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  return JSON.parse(Buffer.concat(chunks).toString());
}
async function withServer(handler, run) {
  const errors = [];
  const server = createServer((request, response) => {
    Promise.resolve().then(() => {
      assert.equal(request.headers.authorization, 'Bearer test-token');
      return handler(request, response);
    }).catch(error => {
      errors.push(error);
      if (!response.headersSent) response.statusCode = 500;
      response.end('Fake server assertion failed');
    });
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const baseUrl = `http://127.0.0.1:${server.address().port}`;
  try {
    await run(new Sandpiper({ baseUrl: `${baseUrl}/`, token: 'test-token' }), baseUrl);
    assert.deepEqual(errors, []);
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  }
}
async function all(iterator) {
  const values = [];
  for await (const value of iterator) values.push(value);
  return values;
}

test('create, handle info, and destroy use real routes, JSON, auth, and methods', async () => {
  const options = { template: 'node', ttl: { idleMs: 1000, maxMs: 10000 }, metadata: { session: 'test', nested: [null, true] } };
  const requests = [];
  await withServer(async (req, res) => {
    requests.push([req.method, req.url]);
    if (req.method === 'POST') {
      assert.equal(req.headers['content-type'], 'application/json');
      assert.deepEqual(await json(req), options);
      res.writeHead(201).end(JSON.stringify(info));
    } else if (req.method === 'GET') res.end(JSON.stringify(info));
    else res.writeHead(204).end();
  }, async client => {
    const machine = await client.createMachine(options);
    assert.equal(machine.id, id);
    assert.deepEqual(await client.machine(id).info(), info);
    assert.equal(await machine.destroy(), undefined);
  });
  assert.deepEqual(requests, [['POST', '/v1/machines'], ['GET', `/v1/machines/${id}`], ['DELETE', `/v1/machines/${id}`]]);
});

test('exec sends options and aggregates bytes before decoding UTF-8', async () => {
  const options = { cwd: '/tmp', env: { A: 'b' }, stdin: 'input\n', timeoutMs: 500 };
  await withServer(async (req, res) => {
    assert.equal(req.url, `/v1/machines/${id}/exec`);
    assert.equal(req.method, 'POST');
    assert.deepEqual(await json(req), { cmd: ['echo', 'hi'], ...options });
    const bytes = Buffer.from('a🌊z');
    stream(res, [started,
      event('exec.stdout', 3, { exec, data: bytes.subarray(0, 3).toString('base64') }),
      event('exec.stdout', 4, { exec, data: bytes.subarray(3).toString('base64') }),
      event('exec.stderr', 5, { exec, data: Buffer.from('warning').toString('base64') }),
      exited({ code: 7, stdoutTruncated: true, stderrTruncated: true })]);
  }, async client => {
    assert.deepEqual(await client.machine(id).exec(['echo', 'hi'], options), {
      exitCode: 7, stdout: 'a🌊z', stderr: 'warning', stdoutTruncated: true, stderrTruncated: true, timedOut: false,
    });
  });
});

test('raw stdin uses stdinBase64 without stdin', async () => {
  await withServer(async (req, res) => {
    assert.deepEqual(await json(req), { cmd: ['cat'], stdinBase64: 'AP+A' });
    stream(res, [started, exited()]);
  }, async client => { await client.machine(id).exec(['cat'], { stdin: new Uint8Array([0, 255, 128]) }); });
});

test('exec defaults omit optional fields', async () => {
  await withServer(async (req, res) => {
    assert.deepEqual(await json(req), { cmd: ['true'] });
    stream(res, [started, exited()]);
  }, async client => { await client.machine(id).exec(['true']); });
});

test('timeout returns partial output, null exit code, and timedOut', async () => {
  await withServer((_req, res) => stream(res, [started,
    event('exec.stdout', 3, { exec, data: 'aGk=' }), event('exec.timedout', 4, { exec, timeoutMs: 10 })]), async client => {
    assert.deepEqual(await client.machine(id).exec(['sleep', '30']), {
      exitCode: null, stdout: 'hi', stderr: '', timedOut: true, stdoutTruncated: false, stderrTruncated: false,
    });
  });
});

for (const terminal of [event('exec.failed', 3, { exec, reason: 'machine stopped' }), destroyed]) {
  test(`${terminal.type} throws ExecError with terminal event`, async () => {
    await withServer((_req, res) => stream(res, [started, terminal]), async client => {
      await assert.rejects(client.machine(id).exec(['true']), error => {
        assert.ok(error instanceof ExecError);
        assert.deepEqual(error.event, terminal);
        assert.equal(error.message, terminal.reason);
        return true;
      });
    });
  });
}

test('execStream preserves all typed events, blank lines, CRLF and split HTTP chunks', async () => {
  const expected = [started, event('exec.error', 3, { exec, message: '🌊' }), exited()];
  await withServer(async (_req, res) => {
    const body = Buffer.from(`\r\n${expected.map(line).join('').replaceAll('\n', '\r\n')}`);
    const split = body.indexOf(Buffer.from('🌊')) + 1;
    res.write(body.subarray(0, split));
    await delay(5);
    res.end(body.subarray(split));
  }, async client => assert.deepEqual(await all(client.machine(id).execStream(['true'])), expected));
});

test('a final record without a newline is accepted', async () => {
  await withServer((_req, res) => res.end(line(started) + JSON.stringify(exited())), async client => {
    assert.equal((await client.machine(id).exec(['true'])).exitCode, 0);
  });
});

for (const body of [line(started), line(started) + '{"type":', '{bad json}\n', line({ ...exited(), code: undefined }), line(event('exec.unknown', 3, { exec }))]) {
  test(`incomplete or malformed exec fails without repeating POST: ${JSON.stringify(body.slice(-30))}`, async () => {
    let calls = 0;
    await withServer((_req, res) => { calls++; res.end(body); }, async client => {
      await assert.rejects(client.machine(id).exec(['true']));
    });
    assert.equal(calls, 1);
  });
}

for (const [status, body, message] of [[401, '{"error":"wrong token"}', 'wrong token'], [422, 'invalid body', 'invalid body'], [404, '', 'Not Found']]) {
  test(`HTTP ${status} preserves status and server message`, async () => {
    await withServer((_req, res) => res.writeHead(status).end(body), async client => {
      await assert.rejects(client.machine(id).info(), error => error instanceof SandpiperError && error.status === status && error.message === message);
    });
  });
}

test('events replays every event kind and stops after destruction', async () => {
  const events = [event('machine.created', 1, { template: 'node', metadata: null }),
    event('machine.checked', 2, { walls: ['guest:vsock'] }), event('machine.running', 3, { readyMs: 120 }),
    { ...started, seq: 4 }, event('exec.stdout', 5, { exec, data: 'aGk=' }),
    event('exec.stderr', 6, { exec, data: '' }), event('exec.error', 7, { exec, message: 'error' }),
    { ...exited(), seq: 8 }, event('exec.timedout', 9, { exec, timeoutMs: 1 }),
    event('exec.failed', 10, { exec, reason: 'stopped' }), event('machine.failed', 11, { reason: 'stopped' }),
    { ...destroyed, seq: 12 }];
  await withServer((req, res) => {
    assert.equal(req.url, `/v1/events?machine=${id}&from=1`);
    stream(res, events);
  }, async client => assert.deepEqual(await all(client.events(id)), events));
});

for (const mode of ['EOF', 'socket failure', 'partial record']) {
  test(`events resumes with lastSeq + 1 after ${mode}`, async () => {
    const cursors = [];
    await withServer(async (req, res) => {
      const from = new URL(req.url, 'http://localhost').searchParams.get('from');
      cursors.push(from);
      if (cursors.length === 1) {
        res.write(line(started));
        if (mode === 'socket failure') { await delay(20); res.destroy(); }
        else res.end(mode === 'partial record' ? '{"type":"exec.stdout"' : '');
      } else stream(res, [{ ...started, seq: 2 }, exited(), destroyed]);
    }, async client => assert.deepEqual(await all(client.events(id, { from: 2 })), [started, exited(), destroyed]));
    assert.deepEqual(cursors, ['2', '3']);
  });
}

test('empty replay past destruction ends without reconnecting', async () => {
  const paths = [];
  await withServer((req, res) => {
    paths.push(req.url);
    if (req.url.includes('/events?')) res.end();
    else res.end(JSON.stringify({ ...info, state: 'destroyed' }));
  }, async client => assert.deepEqual(await all(client.events(id, { from: 999 })), []));
  assert.deepEqual(paths, [`/v1/events?machine=${id}&from=999`, `/v1/machines/${id}`]);
});

test('events does not retry HTTP errors', async () => {
  let calls = 0;
  await withServer((_req, res) => { calls++; res.writeHead(404).end('{"error":"no machine"}'); }, async client => {
    await assert.rejects(all(client.events(id)), error => error instanceof SandpiperError && error.status === 404);
  });
  assert.equal(calls, 1);
});

for (const method of ['events', 'execStream']) {
  test(`${method} yields stream.error then throws`, async () => {
    const error = { type: 'stream.error', message: 'event log unavailable' };
    await withServer((_req, res) => stream(res, [error]), async client => {
      const iterator = method === 'events' ? client.events(id) : client.machine(id).execStream(['true']);
      assert.deepEqual(await iterator.next(), { value: error, done: false });
      await assert.rejects(iterator.next(), StreamError);
    });
  });
}

test('AbortSignal cancels an open event read', async () => {
  await withServer((_req, res) => res.write(line(started)), async client => {
    const controller = new AbortController();
    const iterator = client.events(id, { signal: controller.signal });
    assert.equal((await iterator.next()).value.seq, 2);
    const pending = iterator.next();
    controller.abort();
    await assert.rejects(pending, { name: 'AbortError' });
  });
});

test('AbortSignal cancels reconnect backoff', async () => {
  await withServer((_req, res) => res.end(line(started)), async client => {
    const controller = new AbortController();
    const iterator = client.events(id, { signal: controller.signal });
    await iterator.next();
    const pending = iterator.next();
    await delay(20);
    controller.abort();
    await assert.rejects(pending, { name: 'AbortError' });
  });
});

test('breaking iteration closes the HTTP stream', async () => {
  let closed;
  const close = new Promise(resolve => { closed = resolve; });
  await withServer((_req, res) => { res.on('close', closed); res.write(line(started)); }, async client => {
    for await (const _event of client.events(id)) break;
    await Promise.race([close, delay(1000).then(() => { throw new Error('stream stayed open'); })]);
  });
});

test('redirects are refused', async () => {
  await withServer((_req, res) => res.writeHead(307, { Location: '/elsewhere' }).end(), async client => {
    await assert.rejects(client.machine(id).exec(['true']), error => error instanceof SandpiperError && error.status === 307);
    await assert.rejects(all(client.events(id)), error => error instanceof SandpiperError && error.status === 307);
  });
});

test('invalid ids and cursors are rejected before HTTP', async () => {
  const client = new Sandpiper({ baseUrl: 'http://127.0.0.1:1', token: 'test-token' });
  for (const value of ['..', 'bad/id', 'm_0123456789ABCDEF']) assert.throws(() => client.machine(value), TypeError);
  for (const from of [0, -1, 1.5, NaN, Infinity]) await assert.rejects(all(client.events(id, { from })), TypeError);
  for (const baseUrl of ['file:///tmp', 'http://user:pass@localhost', 'http://localhost/?x=1', 'http://localhost/#x']) {
    assert.throws(() => new Sandpiper({ baseUrl, token: 'test-token' }), TypeError);
  }
});
