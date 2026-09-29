# sandpiper — design

Status: draft · 2026-09-29

One platform that runs code on hardware we control. The same core runs two
kinds of workload:

- **ephemeral** — untrusted code, seconds to hours, thrown away after.
  "Give the agent a computer."
- **service** — our own apps, weeks to months, with a domain and TLS.
  "Deploy this app."

Both are a **machine**: a microVM started from a **template**, bound by a
**policy**. Only the lifetime, the ingress and the host pool differ.

Sandbox mode ships first. Service mode is built on the same parts afterwards.

---

## 1. Goals

1. A machine boots and accepts a command in under 1 second on warm capacity.
2. Code inside a machine cannot reach the host, other machines, or the network
   beyond what its policy allows.
3. Credentials are usable by code in a machine but never readable by it.
4. Every action taken in or on a machine is recorded in a log that cannot be
   edited without detection.
5. Changes a machine makes to its filesystem can be rolled back.
6. Litro agents can start, drive and chain machines through one typed client.
7. Runs on a single homelab box with no external services, and scales to a
   few rented bare-metal hosts without a redesign.

## 2. Non-goals (for now)

- Multi-region, multi-cloud scheduling.
- Kubernetes. Hosts run one binary each; no cluster manager underneath.
- GPU machines.
- Billing, quotas per customer, a public sign-up. There is one operator.
- Windows guests.

---

## 3. Concepts

| Term | Meaning |
|---|---|
| **Host** | A Linux server with KVM. Runs the host agent. |
| **Pool** | A named group of hosts. `untrusted` and `services` are separate pools, so untrusted code never shares a host with production apps. |
| **Template** | An immutable root filesystem plus defaults (CPU, memory, policy, start command). Built from a Dockerfile or an OCI image. Versioned by content hash. |
| **Snapshot** | A saved machine state. *Disk* snapshots support rollback. *Memory* snapshots of a booted template support fast starts. |
| **Machine** | A running microVM from a template. Has an ID, a mode, a policy, a state. |
| **Mode** | `ephemeral` or `service`. Decides lifetime, restart behaviour and ingress. |
| **Policy** | What a machine may touch: files, network, secrets, resources, time. See §7. |
| **Session** | A caller's use of a machine: the commands run, their output, the audit log. One machine can serve one or more sessions over its life. |

### 3.1 The shore-bird family (proposed)

Sandpipers are a family of shore birds. The platform follows the same idea:
each kind of workload is a member of the family. The names are for the CLI,
the docs and the dashboard. Code and API types keep the plain terms above
(`Machine { mode: Ephemeral }`), so a name can change without an API change.

| Bird | Why | Platform role |
|---|---|---|
| **sanderling** | Runs in with each wave and back out again. | Ephemeral machine. |
| **plover** | Nests in one scrape in the sand and guards it. | Service machine: stays, has a domain. |
| **turnstone** | Flips stones over to find what is under them. | Template builder: turns source into images. |
| **dunlin** | Flies in large, tight flocks that turn as one. | Pool / group of machines run together. |
| **godwit** | Makes the longest non-stop flight of any bird. | Long-running batch job (later). |

Only sanderling (M1) and plover (M4) are committed; the rest are placeholders
until the feature exists.

---

## 4. Architecture

```
            Litro app / CLI / other callers
                        │  HTTPS + streaming
                        ▼
┌──────────────────────────────────────────────────────┐
│ control plane (one process, SQLite)                   │
│  API · scheduler · template registry · policy compiler│
│  secret vault · audit sealer · event bus              │
└───────────────┬───────────────────────────┬──────────┘
                │ mTLS gRPC                 │ routes
                ▼                           ▼
┌──────────────────────────────┐   ┌───────────────────┐
│ host agent (one per host)    │   │ edge proxy        │
│  runtime driver ─► microVMs  │   │  TLS, domains,    │
│  warm pool · snapshots       │   │  preview URLs     │
│  egress proxy (per host)     │   └─────────┬─────────┘
│  log + metrics shipping      │             │
└───────────────┬──────────────┘             │
                │ vsock                      │ tap / bridge
                ▼                            ▼
        ┌────────────────────────────────────────┐
        │ microVM                                 │
        │  guest agent: exec, PTY, files, ports   │
        │  in-guest confinement (second wall)     │
        │  user workload                          │
        └────────────────────────────────────────┘
```

### 4.1 Control plane

One binary. State in SQLite (litestream-style replication later if needed).
Responsibilities:

- **API** — REST for commands, a streaming channel (WebSocket or NDJSON) for
  exec output, PTY and events.
- **Scheduler** — picks a host in the right pool with free capacity and a warm
  machine of the requested template. Bin-packing, nothing clever.
- **Template registry** — stores built root filesystems by content hash and
  pushes them to hosts on demand.
- **Policy compiler** — merges the policy layers (§7.1) into one resolved
  policy per machine, and rejects any layer that tries to widen a parent.
- **Secret vault** — encrypted at rest. Hands secrets only to egress proxies,
  never to machines.
- **Audit sealer** — receives audit records, chains and seals them (§9).

The control plane holds no machine traffic. If it goes down, running machines
keep running; new ones cannot be created.

### 4.2 Host agent

One binary per host. Talks to the control plane over mutual TLS.

- Starts, stops, snapshots and restores microVMs through a **runtime driver**.
- Keeps a **warm pool**: N machines per popular template, booted and paused
  from a memory snapshot, ready to hand out.
- Runs the **egress proxy** for its machines (§7.3, §8).
- Creates one tap device and one network namespace per machine; no machine can
  see another's traffic.
- Enforces CPU, memory, disk and wall-clock limits from outside the guest.
- Ships logs, metrics and audit records to the control plane.

### 4.3 Runtime driver

An interface, so the microVM engine can change without touching the rest:

```
create(template, resources) → vm
start(vm) / pause(vm) / resume(vm) / stop(vm)
snapshot(vm, kind: disk|memory) → snapshot
restore(snapshot) → vm
```

- **Driver 1 (M1):** a libkrun-based microVM. Small, simple, boots fast, good
  on a single homelab box.
- **Driver 2 (later):** a Firecracker-based microVM, for memory snapshots at
  scale and a smaller attack surface.

Every driver must give each machine its own guest kernel. Container-only
isolation is not an accepted driver for the `untrusted` pool.

### 4.4 Guest agent

A small static binary baked into every template. PID 1's first child. Speaks to
the host agent over **vsock** only — it opens no network port.

- `exec` — run a command, stream stdout/stderr, return exit code.
- `pty` — interactive terminal.
- `files` — read, write, list, upload, download.
- `ports` — report listening ports so the edge proxy can expose them.
- Applies the **in-guest confinement** part of the policy (§7.2) to every
  process it starts.

### 4.5 Edge proxy

Terminates TLS. Routes:

- `<port>-<machine>.preview.<domain>` → an ephemeral machine's port, when its
  policy allows exposure. Short-lived, signed URLs.
- `app.example.com` → a service machine. Certificates issued automatically.

Built on an off-the-shelf proxy with an on-demand TLS feature, configured by
the control plane through its admin API.

---

## 5. Machine lifecycle

```
             create
               │
               ▼
  ┌───────► starting ──fail──► failed
  │            │
  │            ▼
  │         running ◄──resume── paused
  │          │  │  └──pause───►  ▲
  │  restore │  │                │ idle timeout (ephemeral)
  │          │  ▼                │
  └──────── snapshotting ────────┘
               │
          stop / TTL / policy violation
               ▼
            stopped ──► destroyed (disk wiped, audit sealed)
```

| | ephemeral | service |
|---|---|---|
| Default TTL | 15 min idle, 4 h max | none |
| On crash | stays `failed`, caller decides | restarted with backoff |
| On host loss | lost; caller sees `failed` | rescheduled from last disk snapshot |
| Ingress | none, or signed preview URL | domain + TLS |
| Disk | discarded on destroy | persistent volume |
| Pool | `untrusted` | `services` |

A **policy violation** that the host can observe (egress to a blocked domain
repeated past a threshold, a resource limit breached) stops the machine and
records why.

---

## 6. API sketch

All calls are authenticated with an API key scoped to a project. Machine IDs
are opaque.

```
POST   /v1/templates                 build from Dockerfile or OCI ref
GET    /v1/templates/:id

POST   /v1/machines                  { template, mode, policy?, ttl?, metadata? }
GET    /v1/machines/:id
DELETE /v1/machines/:id

POST   /v1/machines/:id/exec         { cmd, cwd?, env?, stdin?, policy? } → stream
GET    /v1/machines/:id/pty          upgrade to WebSocket
GET    /v1/machines/:id/files?path=
PUT    /v1/machines/:id/files?path=
POST   /v1/machines/:id/snapshots    { kind: disk }
POST   /v1/machines/:id/restore      { snapshot }
POST   /v1/machines/:id/expose       { port } → signed preview URL

GET    /v1/machines/:id/audit        records + seal
GET    /v1/events?machine=&from=     replayable event stream

# service mode (M4)
POST   /v1/services                  { template, domains, env, volume, replicas: 1 }
POST   /v1/services/:id/deploy       { template }  → new machine, health check, swap
```

**Events are durable and replayable.** Each event has a sequence number; a
client that disconnects reconnects with `from=<seq>` and misses nothing. This
matches how Litro sessions already work, so a sandbox job survives a browser
refresh end to end.

A TypeScript client (`@beatzball/sandpiper`) wraps this API. It is the only
thing Litro depends on.

---

## 7. Policy

A policy is **what a machine may touch**. It has five parts:

```ts
interface Policy {
  fs: {
    read:  string[];          // globs, inside the guest
    write: string[];
    deny:  string[];          // wins over read/write
  };
  net: {
    egress: 'none' | 'allowlist' | 'open';
    allow: string[];          // domains, optionally with ports and path prefixes
    expose: boolean;          // may request preview URLs
  };
  secrets: SecretGrant[];     // see §8
  resources: { cpus: number; memoryMb: number; diskMb: number; pids: number };
  time: { idleMs: number; maxMs: number; execMaxMs: number };
}
```

### 7.1 Layers, and narrowing only

Policies stack. Each layer can only **narrow** the one above it, never widen:

```
pool  →  template  →  machine  →  exec (one command)
```

- A pool sets the ceiling (the `untrusted` pool never allows `egress: 'open'`).
- A template sets what its software needs (a Python template may write `/tmp`
  and `/work`).
- A machine narrows for one job.
- A single `exec` can narrow further for one command — for example, a tool that
  only reads files runs with `write: []` and `egress: 'none'` even though the
  machine allows more.

The compiler resolves the stack when the machine is created and when each exec
starts. A request that widens a parent is **rejected**, not silently clipped,
so the caller learns the rule.

Once applied inside a running process, a policy **cannot be loosened** by that
process or its children. Loosening requires a new exec from outside.

### 7.2 Two walls

| Wall | Where | Stops |
|---|---|---|
| **microVM** | host agent | Anything reaching the host kernel, other machines, host files. The primary boundary. |
| **in-guest confinement** | guest agent | A process touching files or sockets inside the guest that its exec policy does not allow. Uses kernel features in the guest (filesystem access rules, syscall filters, no new privileges). |

The second wall matters because one machine often runs several steps with
different trust — the agent's own code, a package install, a test run — and
per-exec policy only works if the guest enforces it.

### 7.3 Network

- Each machine's only route out is the host's **egress proxy**. No direct
  internet, no route to other machines, the host, or the LAN.
- The proxy enforces the domain allow-list at connect time (TLS SNI / HTTP
  host), and optionally path prefixes for HTTP.
- DNS is answered by the proxy; blocked names get no answer.
- Every allowed and denied connection is an audit record.

---

## 8. Secrets: usable, never readable

Code in a machine needs to call paid APIs. It must not be able to print, save
or send the key anywhere else.

1. A **secret grant** names a secret, the hosts it is valid for, and how it is
   injected:
   ```ts
   { name: 'ANTHROPIC_API_KEY', hosts: ['api.anthropic.com'], inject: { header: 'x-api-key' } }
   ```
2. The machine receives a **placeholder** in its environment, for example
   `ANTHROPIC_API_KEY=am_ph_7f3c…`. The placeholder is worthless outside this
   machine.
3. The machine's HTTPS traffic to granted hosts goes through the egress proxy,
   which trusts a per-machine CA installed in the template.
4. The proxy swaps the placeholder for the real value **only** on requests to
   the hosts in the grant. A placeholder sent to any other host is sent as-is
   and logged as a probe.
5. The real value lives in the control-plane vault and in the proxy's memory.
   It never enters the guest, its disk, its snapshots or its logs.

Revoking a grant takes effect on the next request, with no machine restart.

---

## 9. Audit log

Every action is a record: machine created, exec started, exec finished (exit
code, duration), file written through the API, egress allowed or denied,
secret injected (name and host, never value), policy violation, snapshot,
restore, destroy.

- Records are **append-only** and **hash-chained**: each carries the hash of
  the one before it.
- When a machine is destroyed — or every N minutes for a service — the chain is
  **sealed**: a Merkle root over the segment is computed and signed with the
  control plane's key.
- `GET /v1/machines/:id/audit` returns the records and the seal. Anyone with
  the public key can check that nothing was removed, reordered or edited.

This answers "what did the agent actually do?" after the fact, with proof.

---

## 10. Snapshots and rollback

- **Before-exec snapshots.** An exec can ask for `snapshot: true`. The host
  takes a copy-on-write disk snapshot first; if the result is bad, the caller
  restores it. This gives agents an undo button for destructive steps.
- **Branching.** Restoring a snapshot into a *new* machine forks the state, so
  an agent can try two approaches from the same point.
- **Warm starts.** A template's booted memory snapshot lets a new machine
  resume instead of boot (driver 2; driver 1 uses a paused warm pool).

Snapshots are bound by the same policy as their machine and are deleted with
it unless pinned.

---

## 11. Litro integration

Litro agents use machines through ordinary tools. Nothing in `litro-agent`
changes; the client package is all that is needed.

### 11.1 One machine per agent session

```ts
// agents/coder/tools/run-code.ts
import { defineTool } from '@beatzball/litro-agent';
import { ui } from '@beatzball/litro-agent/ui';
import { html } from 'lit';
import { machines } from '../../../server/machines.js';
import { RunOutput } from '../../../components/run-output.js';

void RunOutput;

export default defineTool({
  description: 'Run a shell command in this session’s sandbox',
  input: runSchema,
  async execute({ cmd }, ctx) {
    const m = await machines.forSession(ctx.session.id, { template: 'node-22' });
    const r = await m.exec(cmd, { snapshot: true });
    return ui(html`<run-output .cmd=${cmd} .result=${r}></run-output>`, {
      data: { exitCode: r.exitCode, stdout: r.stdout.slice(-8000), stderr: r.stderr.slice(-4000) },
    });
  },
});
```

- `forSession` reuses the machine bound to this Litro session, or creates one.
  The machine ID is stored against the session, so a reconnect finds it again.
- The model sees trimmed `data`; the page gets the full rendered output.
- Litro's generator-return tools can stream exec output to the page as it
  arrives.

### 11.2 Chains

A tool can run a sequence across machines: build in one, test in several in
parallel, then expose the result as a preview URL. Each step carries its own
narrowed exec policy.

### 11.3 From sandbox to service

Because both modes share templates, a machine an agent built up can be
snapshotted into a template and deployed as a service with one more call. This
is the path from "the agent made an app" to "the app is live".

---

## 12. Hosting

Every host needs **KVM**: bare metal, or a cloud VM with nested
virtualization.

| Stage | Where | Why |
|---|---|---|
| M0–M3 | Homelab Linux box | Free, fast iteration, full control. |
| M4+ | One or two rented bare-metal hosts (Hetzner dedicated, OVHcloud, Latitude.sh, Vultr Bare Metal, Scaleway Elastic Metal) | Public reach, real hardware, low monthly cost. |
| Only if needed | GCP or AWS with nested virtualization / `.metal` | Burst capacity; most expensive. |

Shared-vCPU cloud VMs without nested virtualization cannot run this.

Pools map to hosts: the homelab can hold both pools at first; once rented
hosts exist, `untrusted` and `services` move to different machines.

---

## 13. Milestones

| | Scope | Done when |
|---|---|---|
| **M0** | Spike on the homelab box: boot a microVM with driver 1, run a command over vsock, get output. | A script prints `hello` from inside a VM in < 1 s warm. |
| **M1** | Host agent + control plane (single host), templates from Dockerfile, `create/exec/files/destroy`, TTLs, event stream with replay. TS client. | Litro `run-code` tool works end to end in the playground. |
| **M2** | Policy: layers, narrowing, in-guest confinement, egress proxy with allow-list, resource limits. | A test suite of escape attempts all fail and all appear in the log. |
| **M3** | Secrets via placeholders, audit chain + seals, disk snapshots and restore. | An agent calls a paid API with a key it cannot print; audit verifies offline. |
| **M4** | Service mode: persistent volumes, edge proxy with domains and TLS, deploy with health-checked swap, restart on crash. Second host, separate pools. | A Litro app is deployed and served on its own domain from this platform. |
| **M5** | Driver 2, memory snapshots, branching, preview URLs, multi-host scheduling. | Cold-from-snapshot start < 300 ms; two hosts share load. |

---

## 14. Open questions

1. ~~**Language for control plane and host agent.**~~ **Decided: Rust** for
   the control plane, host agent and guest agent. The microVM engines and the
   in-guest confinement libraries are Rust-native, so the security-critical
   parts need no C bridge. The client stays in TypeScript.
2. **Guest base images.** Build from Dockerfiles (familiar, reuses existing
   images) or from a minimal fixed base plus packages (smaller, faster boot).
   Recommendation: Dockerfiles, with a slim default template.
3. **Egress TLS interception.** Needed for secret injection and path rules.
   Alternative for services that should not be intercepted: SNI-only
   allow-listing and no secret grants. Probably both, chosen per grant.
4. **Where builds run.** Template builds execute untrusted Dockerfiles; they
   should run inside a machine in the `untrusted` pool, not on the host.
5. **Auth model beyond one operator.** Project-scoped API keys are enough now.
   Revisit if others get access.
6. ~~**Name.**~~ **Decided: sandpiper.** Crate `sandpiper`, npm
   `@beatzball/sandpiper`, repo `beatzball/sandpiper`.
