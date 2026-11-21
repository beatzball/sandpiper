import assert from 'node:assert/strict';
import test from 'node:test';
import { Sandpiper } from '../dist/index.js';

const baseUrl = process.env.SANDPIPER_URL;

test('real sandpiperd client integration', { skip: !baseUrl && 'SANDPIPER_URL is unset', timeout: 900_000 }, async t => {
  const token = process.env.SANDPIPER_TOKEN;
  assert.ok(token, 'Set SANDPIPER_TOKEN when SANDPIPER_URL is set');
  const signal = AbortSignal.timeout(process.env.SANDPIPER_LONG === '1' ? 890_000 : 110_000);
  const client = new Sandpiper({ baseUrl, token });
  const long = process.env.SANDPIPER_LONG === '1';
  const { machine, info: created } = await client.createMachine({ template: 'node', ttl: { idleMs: long ? 600_000 : 30_000, maxMs: long ? 900_000 : 120_000 }, metadata: { test: 'typescript-client' } }, { signal });
  assert.equal(created.id, machine.id);
  let destroyed = false;
  try {
    await t.test('health reports ok', async () => {
      assert.deepEqual(await client.health({ signal }), { ok: true });
    });
    await t.test('create and info report a running node machine', async () => {
      const info = await machine.info({ signal });
      assert.equal(info.state, 'running');
      assert.equal(info.template, 'node');
      assert.equal(client.machine(machine.id).id, machine.id);
    });
    await t.test('node runs inside the machine and returns 2', async () => {
      const result = await machine.exec(['node', '-e', 'console.log(1+1)'], { signal });
      assert.deepEqual(result, { exitCode: 0, stdout: '2\n', stderr: '', stdoutTruncated: false, stderrTruncated: false, timedOut: false, errors: [] });
    });
    await t.test('cwd, env, text stdin, and nonzero exit code', async () => {
      const result = await machine.exec(['sh', '-c', 'printf "%s:%s:" "$KEY" "$PWD"; cat; printf warning >&2; exit 7'], {
        cwd: '/tmp', env: { KEY: 'value' }, stdin: 'input', timeoutMs: 3000, signal,
      });
      assert.equal(result.stdout, 'value:/tmp:input');
      assert.equal(result.stderr, 'warning');
      assert.equal(result.exitCode, 7);
    });
    await t.test('binary stdin and streaming events', async () => {
      const chunks = [];
      let terminal;
      for await (const event of machine.execStream(['cat'], { stdin: new Uint8Array([0, 255, 128]), signal })) {
        if (event.type === 'exec.stdout') chunks.push(Buffer.from(event.data, 'base64'));
        terminal = event;
      }
      assert.deepEqual(Buffer.concat(chunks), Buffer.from([0, 255, 128]));
      assert.equal(terminal.type, 'exec.exited');
      assert.equal(terminal.code, 0);
    });
    await t.test('timeout returns timedOut and the next exec works', async () => {
      const timeout = await machine.exec(['sleep', '30'], { timeoutMs: 500, signal });
      assert.equal(timeout.timedOut, true);
      assert.equal(timeout.exitCode, null);
      assert.equal(timeout.stdoutTruncated, null);
      assert.equal((await machine.exec(['true'], { signal })).exitCode, 0);
    });
    await t.test('stdout output cap is visible in the result', async () => {
      const result = await machine.exec(['node', '-e', 'process.stdout.write("x".repeat(2*1024*1024))'], { timeoutMs: 5000, signal });
      assert.equal(result.exitCode, 0);
      assert.equal(Buffer.byteLength(result.stdout), 1024 * 1024);
      assert.equal(result.stdoutTruncated, true);
    });
    await t.test('a command that cannot start reports why in errors', async () => {
      const result = await machine.exec(['/no/such/command'], { timeoutMs: 5000, signal });
      assert.notEqual(result.exitCode, 0);
      assert.ok(result.errors.length > 0, JSON.stringify(result));
    });
    await t.test('a quiet exec longer than the 300 s idle body timeout completes', { skip: !long && 'set SANDPIPER_LONG=1 (takes about 6 minutes)' }, async () => {
      const result = await machine.exec(['sh', '-c', 'sleep 330; echo done'], { timeoutMs: 400_000, signal });
      assert.equal(result.exitCode, 0);
      assert.equal(result.stdout, 'done\n');
    });
    await t.test('the machine cannot reach the internet', async () => {
      const result = await machine.exec(['wget', '-T', '3', '-O', '/dev/null', 'http://1.1.1.1'], { timeoutMs: 5000, signal });
      assert.equal(result.timedOut, false);
      assert.notEqual(result.exitCode, 0);
      assert.match(result.stderr, /network|unreachable|connect|route/i);
    });
    await machine.destroy({ signal });
    destroyed = true;
    await t.test('destroy, replay, from cursor, and replay past the end', async () => {
      assert.equal((await machine.info({ signal })).state, 'destroyed');
      const events = [];
      for await (const event of client.events(machine.id, { signal })) events.push(event);
      assert.equal(events[0].type, 'machine.created');
      assert.equal(events.at(-1).type, 'machine.destroyed');
      assert.ok(events.some(event => event.type === 'machine.checked'));
      assert.ok(events.some(event => event.type === 'exec.exited'));
      assert.ok(events.some(event => event.type === 'exec.timedout'));
      for (let i = 0; i < events.length; i++) assert.equal(events[i].seq, i + 1);
      const tail = [];
      for await (const event of client.events(machine.id, { from: events.at(-1).seq, signal })) tail.push(event);
      assert.deepEqual(tail, [events.at(-1)]);
      const beyond = [];
      for await (const event of client.events(machine.id, { from: events.at(-1).seq + 100, signal })) beyond.push(event);
      assert.deepEqual(beyond, []);
    });
  } finally {
    if (!destroyed) await machine.destroy({ signal: AbortSignal.timeout(10_000) });
  }
});
