export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export interface SandpiperOptions {
  /** The host's HTTP(S) URL, for example `http://127.0.0.1:7171`. */
  baseUrl: string;
  /** The host's bearer token: printable ASCII, no spaces. */
  token: string;
}

export interface RequestOptions {
  /** Cancel the HTTP request. An accepted exec continues on the host. */
  signal?: AbortSignal | undefined;
}

export interface CreateMachineOptions {
  template: string;
  ttl?: { idleMs?: number | undefined; maxMs?: number | undefined } | undefined;
  metadata?: JsonValue | undefined;
}

export interface ExecOptions extends RequestOptions {
  cwd?: string | undefined;
  env?: Record<string, string> | undefined;
  /** UTF-8 text or raw bytes; bytes travel as stdinBase64. */
  stdin?: string | Uint8Array | undefined;
  /** The host's deadline for the command. The host caps it. */
  timeoutMs?: number | undefined;
  /**
   * Failed reconnects in a row before a dropped exec stream throws the last
   * `ConnectionError`. A received event resets the count. Defaults to 10.
   * `Infinity` resumes until `signal` aborts.
   */
  maxRetries?: number | undefined;
}

export interface EventsOptions extends RequestOptions {
  /** Inclusive per-machine sequence number. Defaults to 1. */
  from?: number | undefined;
  /**
   * Failed connections in a row before `events()` throws the last
   * `ConnectionError`. A received event resets the count. Defaults to
   * `Infinity`: reconnect until `signal` aborts.
   */
  maxRetries?: number | undefined;
  /** Called before each reconnect with the error and the attempt number. */
  onRetry?: ((error: Error, attempt: number) => void) | undefined;
}

export interface MachineInfo {
  id: string;
  state: 'starting' | 'running' | 'stopping' | 'destroyed' | 'failed';
  reason: string | null;
  template: string | null;
  createdAt: number | null;
  /** Present while the machine is in memory on the host. */
  expiresAt?: number;
  idleMs?: number;
}

export interface Health {
  /** Whether the host can start machines. */
  ok: boolean;
  /** Why not, when `ok` is false. */
  reason?: string;
}

export interface ExecResult {
  /** Null when the host ends the exec with exec.timedout. */
  exitCode: number | null;
  stdout: string;
  stderr: string;
  /** Whether the host cut the output. Null after a timeout: the host does not say. */
  stdoutTruncated: boolean | null;
  stderrTruncated: boolean | null;
  timedOut: boolean;
  /** Messages from exec.error events, for example why the command could not start. */
  errors: string[];
}

export interface EventBase {
  machine: string;
  seq: number;
  /** Milliseconds since the Unix epoch. */
  time: number;
}

type Event<T extends string, Fields = object> = EventBase & { type: T } & Fields;

export type MachineEvent =
  | Event<'machine.created', { template: string; metadata: JsonValue }>
  | Event<'machine.checked', { walls: string[] }>
  | Event<'machine.running', { readyMs: number }>
  | Event<'machine.failed', { reason: string }>
  | Event<'machine.destroyed', { reason: string }>;

export type ExecEvent =
  | Event<'exec.started', { exec: string; cmd: string[] }>
  | Event<'exec.stdout' | 'exec.stderr', { exec: string; data: string }>
  | Event<'exec.error', { exec: string; message: string }>
  | Event<'exec.exited', { exec: string; code: number; stdoutTruncated: boolean; stderrTruncated: boolean }>
  | Event<'exec.timedout', { exec: string; timeoutMs: number }>
  | Event<'exec.failed', { exec: string; reason: string }>;

/** Every event type this client knows. Unknown types from a newer host are skipped. */
export type SandpiperEvent = MachineEvent | ExecEvent;
export type MachineDestroyedEvent = Extract<MachineEvent, { type: 'machine.destroyed' }>;
export type ExecStreamEvent = ExecEvent | MachineDestroyedEvent;
/** The terminal events that mean the command did not complete. */
export type ExecFailureEvent = Extract<ExecEvent, { type: 'exec.failed' }> | MachineDestroyedEvent;
