# @beatzball/sandpiper

A strict TypeScript, ESM client for the M1 sandpiperd HTTP API. Requires
Node.js 22 or later. It has no runtime dependencies. This package is local
and is not published to npm.

Build from this directory:

```sh
npm ci
npm run build
```

For a neighboring application, use a relative `file:` dependency pointing to
this directory. Build the client before installing the application. For
example, sibling `sandpiper/` and `app/` checkouts can use
`"@beatzball/sandpiper": "file:../sandpiper/clients/ts"` in the app's
package.json. Adjust the relative path to the checkouts' layout.

```ts
import { Sandpiper, type ExecResult } from '@beatzball/sandpiper';

const client = new Sandpiper({
  baseUrl: process.env.SANDPIPER_URL!, // e.g. http://127.0.0.1:7171
  token: process.env.SANDPIPER_TOKEN!,
});
const machine = await client.createMachine({
  template: 'node',
  ttl: { idleMs: 60_000, maxMs: 600_000 },
  metadata: { session: 'example' },
});
try {
  console.log(await machine.info());
  const result: ExecResult = await machine.exec(
    ['node', '-e', 'console.log(1+1)'],
    { cwd: '/tmp', env: { EXAMPLE: 'value' }, timeoutMs: 5000 },
  );
  console.log(result.stdout); // 2\n
} finally {
  await machine.destroy();
}
```

`createMachine()` returns a `Machine` handle. `client.machine(id)` restores
one without making an HTTP request. The handle has `id`, `info()`,
`destroy()`, `exec()`, and `execStream()`. `info()` returns the current host
state, including `destroyed` or `failed` after a machine ends. `expiresAt`
and `idleMs` are present only while the host keeps the machine in memory.

Commands are argument arrays. For shell syntax, pass `['sh', '-c', script]`.
`stdin` accepts a string or `Uint8Array`; bytes travel as `stdinBase64`.
`exec()` returns `exitCode`, `stdout`, `stderr`, `stdoutTruncated`,
`stderrTruncated`, and `timedOut`. Output is decoded as UTF-8 after joining
its byte chunks. A nonzero exit code is a normal result. A timeout returns
`exitCode: null`, `timedOut: true`, and any output received. Truncation flags
come from `exec.exited`; timeout events do not carry these flags, so both
are false for a timeout and do not certify that its output is complete.

`SandpiperError` has an HTTP `status`, such as 404 or 409, and the host's
error message. `ExecError` has the terminal `event` and means the command
failed to complete (`exec.failed` or `machine.destroyed`). `StreamError`
means an invalid or incomplete event stream. Transport failures can also
throw the built-in fetch errors. Requests are never redirected or retried,
except the replayable event GET described below. A lost exec response does
not cause a second command submission.

For raw typed events, including base64 `data` for output:

```ts
for await (const event of machine.execStream(['echo', 'hello'])) {
  if (event.type === 'exec.stdout') {
    process.stdout.write(Buffer.from(event.data, 'base64'));
  }
}
```

`execStream()` ends after `exec.exited`, `exec.timedout`, `exec.failed`, or
`machine.destroyed`. Callers must inspect the terminal event. A connection
that ends before a terminal event throws. If the command has already been
accepted, it continues on the host. Its events can be recovered from the
machine log, filtered by the `exec` id from `exec.started`.

```ts
const controller = new AbortController();
for await (const event of client.events(machine.id, {
  from: 1,
  signal: controller.signal,
})) {
  if (event.type !== 'stream.error') console.log(event.seq, event.type);
}
```

`from` is an inclusive per-machine sequence number. `events()` replays and
then follows live events. On a dropped connection or premature EOF it
reconnects with `from=lastSeq+1`, with a delay that grows from 100 ms to
5 seconds. It skips duplicate records. It ends after `machine.destroyed`,
or an empty replay past a finished machine's last event. HTTP errors,
invalid records, and `stream.error` are surfaced instead of retried. A
`stream.error` is yielded before `StreamError` is thrown; it has only
`type` and `message`, without sequence or machine fields.

All requests support `signal`. `createMachine()` takes it in a second
options argument. Breaking out of an iterator closes its response stream;
use an `AbortSignal` to cancel a pending read or reconnect delay. Canceling
HTTP does not stop an accepted command or destroy its machine. Use
`timeoutMs` for a command deadline and `destroy()` for machine cleanup.

Run the checks:

```sh
npm run typecheck
npm test
npm run test:integration  # skips when SANDPIPER_URL is unset
```

For a real host, set `SANDPIPER_URL` and `SANDPIPER_TOKEN`, then run
`npm run test:integration`. The host must have the `node` template. The
script creates a machine, runs Node, checks stdin and output caps, tests a
timeout, checks that internet access fails, destroys the machine, and
replays its log. See [host setup](../../docs/host-setup.md#42-run-the-host-agent)
for daemon setup and Lima access from a Mac.
