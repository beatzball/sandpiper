# @beatzball/sandpiper-client

A typed client for the [sandpiper](https://github.com/beatzball/sandpiper)
host API. Create a microVM, run commands in it, and follow its event log.

- Node.js 22.18 or later (the version CI tests). Node only, not browsers: it uses `Buffer` and
  `node:timers/promises`.
- ESM only. There is no CommonJS build; use `import`.
- Types need TypeScript 5.0 or later, with `@types/node` (or the DOM lib)
  for `Response` and `AbortSignal`.
- No runtime dependencies.

```sh
npm install @beatzball/sandpiper-client
```

## Example

```ts
import { Sandpiper } from '@beatzball/sandpiper-client';

const client = new Sandpiper({
  baseUrl: process.env.SANDPIPER_URL!, // e.g. http://127.0.0.1:7171
  token: process.env.SANDPIPER_TOKEN!,
});
const { machine, info } = await client.createMachine({
  template: 'node',
  ttl: { idleMs: 60_000, maxMs: 600_000 },
  metadata: { session: 'example' },
});
console.log(info.state); // running
try {
  const result = await machine.exec(
    ['node', '-e', 'console.log(1+1)'],
    { cwd: '/tmp', env: { EXAMPLE: 'value' }, timeoutMs: 5000 },
  );
  console.log(result.stdout); // 2\n
} finally {
  // The machine may already be gone (TTL, crash). Do not let that hide
  // the error from the try block.
  await machine.destroy().catch(() => {});
}
```

## Machines

`createMachine()` returns `{ machine, info }`: a `Machine` handle and the
host's `MachineInfo`. `client.machine(id)` gets a handle for an existing
machine without an HTTP request. A handle has `id`, `info()`, `destroy()`,
`exec()`, and `execStream()`. `info()` returns the current host state,
including `destroyed` or `failed` after a machine ends. `expiresAt` and
`idleMs` are present only while the host keeps the machine in memory.

`client.health()` returns `{ ok, reason? }`: whether the host can start
machines.

## Commands

Commands are argument arrays. For shell syntax, pass `['sh', '-c', script]`.
`stdin` accepts a string or `Uint8Array`; bytes travel as `stdinBase64`.
The host caps the request body at 16 MiB, so raw stdin is limited to about
12 MiB after base64.

`exec()` returns:

| field | meaning |
|---|---|
| `exitCode` | the exit code; `null` after a timeout |
| `stdout`, `stderr` | output, decoded as UTF-8 after joining its bytes |
| `stdoutTruncated`, `stderrTruncated` | the host cut the stream (at 1 MiB); `null` after a timeout, because the host does not say |
| `timedOut` | the host ended the command at `timeoutMs` |
| `errors` | `exec.error` messages, for example why a command could not start |

A nonzero exit code is a normal result, not an error.

For raw typed events, including base64 `data` for output:

```ts
for await (const event of machine.execStream(['echo', 'hello'])) {
  if (event.type === 'exec.stdout') {
    process.stdout.write(Buffer.from(event.data, 'base64'));
  }
}
```

`execStream()` ends after `exec.exited`, `exec.timedout`, `exec.failed`, or
`machine.destroyed`. The command is submitted once and never again. If the
connection drops after `exec.started`, the client reads the rest of that
exec from the machine's event log, so a long quiet command keeps going. A
clean end of the stream before a terminal event counts as a drop. It gives
up after `maxRetries` failed reconnects in a row (default 10; a received
event resets the count) and throws `ConnectionError`; the command still runs
on the host.

## Events

```ts
const controller = new AbortController();
for await (const event of client.events(machine.id, {
  from: 1,
  signal: controller.signal,
})) {
  console.log(event.seq, event.type);
}
```

`events()` replays a machine's log from `from` (an inclusive sequence
number, default 1), then follows live events. It ends after
`machine.destroyed`, or at once for a cursor past a finished machine's last
event. A cursor past the end of a running machine's log starts at its end,
so the first event can have a lower `seq` than `from`.

After a dropped connection, `events()` reconnects with `from=lastSeq+1`. The
delay grows from 100 ms to 5 s. It skips duplicates. `maxRetries` limits the
failed reconnects in a row (default: no limit); a received event resets the
count. `onRetry(error, attempt)` is called before each reconnect. Only
`ConnectionError` and a stream cut inside a record are retried. HTTP errors,
invalid records, and a host `stream.error` are thrown.

Event types this client does not know are skipped, so a newer host can add
types without breaking it.

## Errors

| class | meaning |
|---|---|
| `SandpiperError` | the host answered with an HTTP error; has `status` (such as 404 or 409) and the host's message |
| `ExecError` | the command did not complete; `event` is the `exec.failed` or `machine.destroyed` event |
| `StreamError` | the host's response is invalid, too large, or incomplete, or the host sent `stream.error` |
| `ConnectionError` | the connection failed or dropped; `cause` has the fetch error. The host may still have acted |
| `TypeError` | invalid arguments, found before any request |

Requests are never redirected. Only `events()` and the exec resume retry,
and both only re-read the log.

The client limits what it accepts from a host: one NDJSON record to 4 Mi
characters, an error body to 64 KiB, a JSON body to 1 MiB, `exec()`
output to 16 MiB per stream, and `exec()` errors to 256 messages or 1 MiB.
A real host stays far below all of these.

## Cancellation and timeouts

Every call takes a `signal`. `createMachine()` takes it in a second options
argument. There is no default deadline; use `AbortSignal.timeout(ms)` for
one. Breaking out of an iterator closes its response stream. Canceling HTTP
does not stop an accepted command or destroy its machine. Use `timeoutMs`
for a command deadline and `destroy()` for cleanup. If a `createMachine()`
call is canceled after the host has created the machine, the machine lives
until its TTL.

The host writes a blank line on a quiet event stream every 30 seconds, so
Node's 300-second idle body timeout does not end a long command. Older
hosts do not. On them each quiet connection ends after 300 seconds and
counts as a failed reconnect, so with the default `maxRetries` of 10 a
command that prints nothing survives about an hour. Pass a larger
`maxRetries`, or `Infinity`, for longer quiet commands.

## Development

From `clients/ts` in the [repository](https://github.com/beatzball/sandpiper):

```sh
npm ci
npm run typecheck
npm test
npm run test:integration  # skips when SANDPIPER_URL is unset
```

For a real host, set `SANDPIPER_URL` and `SANDPIPER_TOKEN`, then run
`npm run test:integration`. The host must have the `node` template. See
[host setup](https://github.com/beatzball/sandpiper/blob/main/docs/host-setup.md#42-run-the-host-agent)
for the daemon and Lima access from a Mac.

## Releases

Releases come from GitHub Actions through npm trusted publishing (OIDC), not
from a laptop and not with an npm token. The workflow is
`.github/workflows/publish-client.yml`. It runs in the GitHub environment
`npm`, publishes with provenance, and needs npm 11.5.1 or later.

**One-time setup.** npm adds a trusted publisher only to a package that
already exists. So the owner publishes one placeholder by hand, once. From
`clients/ts`, logged in to npm as an owner of `@beatzball`:

```sh
npm version 0.0.0-bootstrap.0 --no-git-tag-version
npm publish --tag bootstrap --provenance=false --access public
git checkout -- package.json package-lock.json
```

`--provenance=false` is needed because `publishConfig` asks for provenance,
which only a CI run can make. Then, on npmjs.com, open the package's
settings and add a trusted publisher:

| field | value |
|---|---|
| publisher | GitHub Actions |
| organization or user | `beatzball` |
| repository | `sandpiper` |
| workflow filename | `publish-client.yml` |
| environment | `npm` |
| allowed actions | tick **npm publish** (`npm stage publish` alone is the default for new publishers, and the workflow runs `npm publish`) |

The first trusted publish must happen within 2 days of adding the
publisher. In GitHub, give the `npm` environment a required reviewer and a
deployment tag rule for `client-v*`.

After the first real version ships, mark the placeholder so nobody installs it:

```sh
npm deprecate @beatzball/sandpiper-client@0.0.0-bootstrap.0 "placeholder for trusted publishing setup"
```

**Each release.** Set the version in `package.json`. In `CHANGELOG.md`,
replace "unreleased" with the date. Merge to `main`, then push the tag
`client-v<version>` on that commit. A version with a `-` (a prerelease)
goes to the `next` dist-tag; others go to `latest`.

## License

Apache-2.0. See [LICENSE](LICENSE).
