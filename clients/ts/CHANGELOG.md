# Changelog

All notable changes to `@beatzball/sandpiper-client`. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the package
uses [semantic versioning](https://semver.org/). Before 1.0.0, a minor
version can break the API.

To release: replace "unreleased" with the date (YYYY-MM-DD), commit, then
push the `client-v<version>` tag. See "Releases" in the README.

## 0.1.0 - unreleased

First public release.

- `Sandpiper`: `createMachine()` returns `{ machine, info }`, `machine(id)`,
  `health()`, and `events()` with replay, reconnect, `maxRetries` and
  `onRetry`.
- `Machine`: `info()`, `destroy()`, `exec()` and `execStream()`. A dropped
  exec stream resumes from the event log, up to `maxRetries` failed
  reconnects in a row (default 10); the command is never sent twice.
- `ExecResult` has `errors` (from `exec.error`) and truncation flags that
  are `null` after a timeout.
- Errors: `SandpiperError`, `StreamError`, `ConnectionError`, `ExecError`.
- Requires Node.js 22.18 or later.
- Unknown event types are skipped. A host `stream.error` is thrown as
  `StreamError` and not yielded.
- Limits on what a host can send: 4 Mi characters per record, 64 KiB per
  error body, 1 MiB per JSON body, 16 MiB of `exec()` output per stream,
  256 `exec.error` messages or 1 MiB of them per exec.
- The bearer token is kept in a private field and never appears in logs,
  `JSON.stringify`, or error messages.
