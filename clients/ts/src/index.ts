import { setTimeout as delay } from 'node:timers/promises';
import type {
  CreateMachineOptions, EventsOptions, ExecFailureEvent, ExecOptions, ExecResult, ExecStreamEvent,
  Health, MachineInfo, RequestOptions, SandpiperEvent, SandpiperOptions,
} from './types.js';
export type * from './types.js';

/** The host answered with an HTTP error status. */
export class SandpiperError extends Error {
  override readonly name = 'SandpiperError';
  constructor(message: string, public readonly status: number) {
    super(message);
  }
}

/** The host's response is invalid, too large, or incomplete, or it reported a stream error. */
export class StreamError extends Error {
  override readonly name = 'StreamError';
}

/** The connection failed or dropped. The host may still have acted on the request. */
export class ConnectionError extends Error {
  override readonly name = 'ConnectionError';
}

/** The command did not complete: `exec.failed` or `machine.destroyed`. */
export class ExecError extends Error {
  override readonly name = 'ExecError';
  constructor(message: string, public readonly event: ExecFailureEvent) {
    super(message);
  }
}

/** A stream that ended inside a record: a lost connection, so safe to resume. */
class TruncatedStreamError extends StreamError {}

/** Longest NDJSON record, in characters. Host records stay far below it. */
const MAX_RECORD = 4 << 20;
/** Most bytes read from an error body or a JSON body. */
const MAX_ERROR_BODY = 64 << 10;
const MAX_JSON_BODY = 1 << 20;
/** Most decoded bytes `exec()` keeps per output stream. The host cuts each at 1 MiB. */
const MAX_OUTPUT = 16 << 20;
/** Failed reconnects in a row before a dropped exec stream gives up. */
const EXEC_RESUME_RETRIES = 10;
const TERMINAL = new Set(['exec.exited', 'exec.timedout', 'exec.failed', 'machine.destroyed']);

const resumable = (error: unknown) => error instanceof ConnectionError || error instanceof TruncatedStreamError;

/** Leave aborts and this client's errors alone; anything else is the connection. */
function transportError(error: unknown, signal: AbortSignal | undefined): unknown {
  if (signal?.aborted) return error;
  if (error instanceof SandpiperError || error instanceof StreamError || error instanceof ConnectionError) return error;
  return new ConnectionError('The connection to the host failed', { cause: error });
}

async function readBody(response: Response, limit: number, signal: AbortSignal | undefined) {
  if (!response.body) return { text: '', over: false };
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  let over = false;
  try {
    while (true) {
      let chunk: ReadableStreamReadResult<Uint8Array>;
      try { chunk = await reader.read(); } catch (error) { throw transportError(error, signal); }
      if (chunk.done) break;
      const room = limit - size;
      if (chunk.value.byteLength > room) {
        chunks.push(chunk.value.subarray(0, room));
        over = true;
        break;
      }
      chunks.push(chunk.value);
      size += chunk.value.byteLength;
    }
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
  return { text: Buffer.concat(chunks).toString('utf8'), over };
}

async function readJson(response: Response, signal: AbortSignal | undefined): Promise<Record<string, unknown>> {
  const { text, over } = await readBody(response, MAX_JSON_BODY, signal);
  if (over) throw new StreamError(`The host's JSON response is larger than ${MAX_JSON_BODY} bytes`);
  let value: unknown;
  try { value = JSON.parse(text); } catch { throw new StreamError("The host's response is not JSON"); }
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new StreamError("The host's response is not a JSON object");
  return value as Record<string, unknown>;
}

const STATES = new Set(['starting', 'running', 'stopping', 'destroyed', 'failed']);
const nullable = (value: unknown, type: string) => value === null || typeof value === type;

function machineInfo(value: Record<string, unknown>): MachineInfo {
  const { id, state, reason, template, createdAt, expiresAt, idleMs } = value;
  if (typeof id !== 'string' || !ID.test(id) || typeof state !== 'string' || !STATES.has(state) ||
      !nullable(reason, 'string') || !nullable(template, 'string') || !nullable(createdAt, 'number') ||
      (expiresAt !== undefined && typeof expiresAt !== 'number') || (idleMs !== undefined && typeof idleMs !== 'number')) {
    throw new StreamError('Invalid machine info from the host');
  }
  return value as unknown as MachineInfo;
}

const BASE64 = /^[A-Za-z0-9+/]*={0,2}$/;

/**
 * Check one record. Returns undefined for a type this client does not know,
 * so a newer host can add event types. Throws on a host `stream.error`.
 */
function check(value: unknown): SandpiperEvent | undefined {
  const invalid = (why: string) => new StreamError(`Invalid NDJSON event: ${why}`);
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw invalid('not an object');
  const event = value as Record<string, unknown>;
  const type = event.type;
  if (typeof type !== 'string') throw invalid('missing type');
  if (type === 'stream.error') {
    throw new StreamError(typeof event.message === 'string' ? event.message.slice(0, 4096) : 'The host reported a stream error');
  }
  const strings = (...keys: string[]) => keys.every(key => typeof event[key] === 'string');
  const integer = (key: string) => Number.isSafeInteger(event[key]);
  const stringArray = (key: string) => Array.isArray(event[key]) && event[key].every(item => typeof item === 'string');
  if (!integer('seq') || Number(event.seq) < 1) throw invalid('bad sequence number');
  if (!strings('machine') || !integer('time')) throw invalid('bad envelope');
  let valid: boolean;
  switch (type) {
    case 'machine.created': valid = strings('template') && 'metadata' in event; break;
    case 'machine.checked': valid = stringArray('walls'); break;
    case 'machine.running': valid = integer('readyMs'); break;
    case 'machine.failed':
    case 'machine.destroyed': valid = strings('reason'); break;
    case 'exec.started': valid = strings('exec') && stringArray('cmd'); break;
    case 'exec.stdout':
    case 'exec.stderr': {
      const data = event.data;
      valid = strings('exec') && typeof data === 'string' && data.length % 4 === 0 && BASE64.test(data);
      break;
    }
    case 'exec.error': valid = strings('exec', 'message'); break;
    case 'exec.exited': valid = strings('exec') && integer('code') && typeof event.stdoutTruncated === 'boolean' && typeof event.stderrTruncated === 'boolean'; break;
    case 'exec.timedout': valid = strings('exec') && integer('timeoutMs'); break;
    case 'exec.failed': valid = strings('exec', 'reason'); break;
    default: return undefined;
  }
  if (!valid) throw invalid(`bad fields for ${type}`);
  return event as unknown as SandpiperEvent;
}

/** Parse complete NDJSON records across arbitrary HTTP chunk boundaries. */
async function* records(response: Response, signal: AbortSignal | undefined): AsyncGenerator<SandpiperEvent> {
  if (!response.body) throw new StreamError('The response has no body');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const parse = (line: string) => {
    let value: unknown;
    try { value = JSON.parse(line); } catch { throw new StreamError('Invalid NDJSON event: not JSON'); }
    return check(value);
  };
  let pending = '';
  try {
    while (true) {
      let chunk: ReadableStreamReadResult<Uint8Array>;
      try { chunk = await reader.read(); } catch (error) { throw transportError(error, signal); }
      const text = decoder.decode(chunk.value, { stream: !chunk.done });
      // Search only the new text, so a long record costs linear time.
      let start = 0;
      let newline: number;
      while ((newline = text.indexOf('\n', start)) !== -1) {
        const line = (pending + text.slice(start, newline)).trim();
        pending = '';
        start = newline + 1;
        const event = line ? parse(line) : undefined;
        if (event) yield event;
      }
      pending += text.slice(start);
      if (pending.length > MAX_RECORD) throw new StreamError(`An NDJSON record is longer than ${MAX_RECORD} characters`);
      if (chunk.done) {
        const line = pending.trim();
        if (line) {
          let value: unknown;
          try { value = JSON.parse(line); } catch { throw new TruncatedStreamError('The stream ended inside an NDJSON record'); }
          const event = check(value);
          if (event) yield event;
        }
        return;
      }
    }
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

const ID = /^[A-Za-z0-9_-]{1,128}$/;
function checkId(id: string): void {
  if (typeof id !== 'string' || !ID.test(id)) throw new TypeError('Invalid machine id');
}

/** The HTTP layer. Not exported: the token lives only in this private field. */
class Transport {
  readonly #baseUrl: string;
  readonly #token: string;

  constructor(baseUrl: string, token: string) {
    const url = new URL(baseUrl);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
      throw new TypeError('baseUrl must be an HTTP(S) URL without credentials, query, or fragment');
    }
    // The message never repeats the token.
    if (typeof token !== 'string' || !/^[\x21-\x7e]+$/.test(token)) {
      throw new TypeError('token must be printable ASCII without spaces');
    }
    this.#baseUrl = url.href.replace(/\/$/, '');
    this.#token = token;
  }

  /** HTTP failures are never retried here. */
  async request(path: string, method: string, body: unknown, signal: AbortSignal | undefined): Promise<Response> {
    let response: Response;
    try {
      response = await fetch(`${this.#baseUrl}${path}`, {
        method,
        headers: {
          Authorization: `Bearer ${this.#token}`,
          ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
        },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
        ...(signal === undefined ? {} : { signal }),
        redirect: 'manual',
      });
    } catch (error) {
      throw transportError(error, signal);
    }
    if (!response.ok) {
      let text = '';
      try { text = (await readBody(response, MAX_ERROR_BODY, signal)).text; } catch (error) {
        if (signal?.aborted) throw error;
      }
      let message = text || response.statusText;
      try {
        const parsed = JSON.parse(text) as { error?: unknown };
        if (typeof parsed.error === 'string') message = parsed.error;
      } catch { /* Some framework errors have plain-text bodies. */ }
      throw new SandpiperError(message, response.status);
    }
    return response;
  }

  async info(id: string, signal: AbortSignal | undefined): Promise<MachineInfo> {
    const response = await this.request(`/v1/machines/${id}`, 'GET', undefined, signal);
    return machineInfo(await readJson(response, signal));
  }
}

/**
 * Replay and follow one machine's log, reconnecting from `lastSeq + 1`
 * after a dropped connection.
 */
async function* follow(transport: Transport, machineId: string, options: EventsOptions): AsyncGenerator<SandpiperEvent> {
  const { signal, onRetry } = options;
  const maxRetries = options.maxRetries ?? Infinity;
  let next = options.from ?? 1;
  if (!Number.isSafeInteger(next) || next < 1) throw new TypeError('from must be a positive safe integer');
  if (!(maxRetries === Infinity || (Number.isSafeInteger(maxRetries) && maxRetries >= 0))) {
    throw new TypeError('maxRetries must be a non-negative integer or Infinity');
  }
  // The highest sequence number yielded. A cursor past the end of the log
  // starts at its end, so the first event may be below `from`.
  let last = 0;
  let failures = 0;
  let retryMs = 100;
  while (true) {
    signal?.throwIfAborted();
    try {
      let received = false;
      const response = await transport.request(`/v1/events?machine=${machineId}&from=${next}`, 'GET', undefined, signal);
      for await (const event of records(response, signal)) {
        if (event.machine !== machineId) throw new StreamError('Event belongs to another machine');
        if (event.seq <= last) continue;
        last = event.seq;
        next = event.seq + 1;
        received = true;
        failures = 0;
        retryMs = 100;
        yield event;
        if (event.type === 'machine.destroyed') return;
      }
      // The host closes an empty replay only when the cursor is past a
      // finished machine's last event.
      if (!received) {
        const info = await transport.info(machineId, signal);
        if (info.state === 'destroyed' || info.state === 'failed') return;
      }
      throw new ConnectionError('The event stream ended before machine.destroyed');
    } catch (error) {
      signal?.throwIfAborted();
      if (!resumable(error)) throw error;
      failures++;
      if (failures > maxRetries) throw error;
      onRetry?.(error as Error, failures);
    }
    await delay(retryMs, undefined, signal ? { signal } : {});
    retryMs = Math.min(retryMs * 2, 5_000);
  }
}

export interface CreatedMachine {
  machine: Machine;
  /** The host's answer to the create request. */
  info: MachineInfo;
}

let newMachine: (transport: Transport, id: string) => Machine;
/** Proves a Machine was made here: `client.machine(id)` is the only door. */
const INTERNAL = Symbol('sandpiper.internal');

export class Sandpiper {
  readonly #transport: Transport;

  constructor({ baseUrl, token }: SandpiperOptions) {
    this.#transport = new Transport(baseUrl, token);
  }

  /** A handle for an existing machine. Makes no HTTP request. */
  machine(id: string): Machine {
    checkId(id);
    return newMachine(this.#transport, id);
  }

  async createMachine(options: CreateMachineOptions, request: RequestOptions = {}): Promise<CreatedMachine> {
    const response = await this.#transport.request('/v1/machines', 'POST', options, request.signal);
    const info = machineInfo(await readJson(response, request.signal));
    return { machine: newMachine(this.#transport, info.id), info };
  }

  /** The host's readiness. */
  async health(options: RequestOptions = {}): Promise<Health> {
    const response = await this.#transport.request('/v1/health', 'GET', undefined, options.signal);
    const { ok, reason } = await readJson(response, options.signal);
    if (typeof ok !== 'boolean' || (reason !== undefined && typeof reason !== 'string')) {
      throw new StreamError('Invalid health response from the host');
    }
    return typeof reason === 'string' ? { ok, reason } : { ok };
  }

  /** Replay and follow a machine, reconnecting after a dropped connection. */
  async *events(machineId: string, options: EventsOptions = {}): AsyncGenerator<SandpiperEvent> {
    checkId(machineId);
    yield* follow(this.#transport, machineId, options);
  }
}

export class Machine {
  readonly id: string;
  readonly #transport: Transport;

  private constructor(key: symbol, transport: Transport, id: string) {
    if (key !== INTERNAL) throw new TypeError('Use client.machine(id) to get a Machine');
    this.#transport = transport;
    this.id = id;
  }

  static {
    newMachine = (transport, id) => new Machine(INTERNAL, transport, id);
  }

  async info(options: RequestOptions = {}): Promise<MachineInfo> {
    return await this.#transport.info(this.id, options.signal);
  }

  async destroy(options: RequestOptions = {}): Promise<void> {
    const response = await this.#transport.request(`/v1/machines/${this.id}`, 'DELETE', undefined, options.signal);
    await response.body?.cancel();
  }

  /**
   * Raw wire events of one command. The command is submitted once. If the
   * connection drops after `exec.started`, the rest comes from the event log.
   */
  async *execStream(cmd: readonly string[], options: ExecOptions = {}): AsyncGenerator<ExecStreamEvent> {
    const { signal, stdin, cwd, env, timeoutMs } = options;
    const input = stdin instanceof Uint8Array
      ? { stdinBase64: Buffer.from(stdin).toString('base64') }
      : { stdin };
    const response = await this.#transport.request(`/v1/machines/${this.id}/exec`, 'POST',
      { cmd, cwd, env, timeoutMs, ...input }, signal);
    let execId: string | undefined;
    let last = 0;
    // `fromLog` events are the whole machine's; skip what is not this exec.
    const accept = (event: SandpiperEvent, fromLog: boolean): ExecStreamEvent | undefined => {
      if (event.machine !== this.id) throw new StreamError('Event belongs to another machine');
      last = event.seq;
      if (event.type === 'machine.destroyed') return event;
      if (!event.type.startsWith('exec.')) {
        if (fromLog) return undefined;
        throw new StreamError('Unexpected event in exec stream');
      }
      const execEvent = event as Extract<SandpiperEvent, { exec: string }>;
      if (execId === undefined) {
        if (execEvent.type !== 'exec.started') throw new StreamError('The exec stream did not start with exec.started');
        execId = execEvent.exec;
        return execEvent;
      }
      if (execEvent.exec !== execId || execEvent.type === 'exec.started') {
        if (fromLog) return undefined;
        throw new StreamError('Event belongs to another exec');
      }
      return execEvent;
    };
    try {
      for await (const event of records(response, signal)) {
        const accepted = accept(event, false);
        if (!accepted) continue;
        yield accepted;
        if (TERMINAL.has(accepted.type)) return;
      }
      throw new StreamError('The exec stream ended before a terminal event; use events() to replay');
    } catch (error) {
      // Before exec.started arrives the exec id is unknown, so nothing can resume.
      if (execId === undefined || !resumable(error)) throw error;
    }
    for await (const event of follow(this.#transport, this.id, { from: last + 1, signal, maxRetries: EXEC_RESUME_RETRIES })) {
      const accepted = accept(event, true);
      if (!accepted) continue;
      yield accepted;
      if (TERMINAL.has(accepted.type)) return;
    }
    throw new StreamError('The machine log ended before the exec did');
  }

  async exec(cmd: readonly string[], options: ExecOptions = {}): Promise<ExecResult> {
    const output = { stdout: [] as Buffer[], stderr: [] as Buffer[] };
    const sizes = { stdout: 0, stderr: 0 };
    const errors: string[] = [];
    for await (const event of this.execStream(cmd, options)) {
      if (event.type === 'exec.stdout' || event.type === 'exec.stderr') {
        const name = event.type === 'exec.stdout' ? 'stdout' : 'stderr';
        const bytes = Buffer.from(event.data, 'base64');
        sizes[name] += bytes.byteLength;
        if (sizes[name] > MAX_OUTPUT) throw new StreamError(`The exec's ${name} is larger than ${MAX_OUTPUT} bytes`);
        output[name].push(bytes);
      }
      if (event.type === 'exec.error') errors.push(event.message);
      if (event.type === 'exec.failed' || event.type === 'machine.destroyed') {
        throw new ExecError(event.reason, event);
      }
      if (event.type === 'exec.exited' || event.type === 'exec.timedout') {
        const exited = event.type === 'exec.exited';
        return {
          exitCode: exited ? event.code : null,
          stdout: Buffer.concat(output.stdout).toString('utf8'),
          stderr: Buffer.concat(output.stderr).toString('utf8'),
          stdoutTruncated: exited ? event.stdoutTruncated : null,
          stderrTruncated: exited ? event.stderrTruncated : null,
          timedOut: !exited,
          errors,
        };
      }
    }
    throw new StreamError('Exec ended without a result');
  }
}
