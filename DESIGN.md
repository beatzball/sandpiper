# sandpiper — design

Status: draft · revised 2026-10-23 after two review rounds

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
2. On a Linux KVM host, code inside a machine cannot reach the host, other
   machines, or the network beyond what its policy allows. (A macOS
   development host makes no such claim — see §12.)
3. Credentials are usable by code in a machine but never handed to it, within
   the trust limits stated in §8.
4. Every API action on a machine and every egress decision the platform makes
   for it is recorded in a log whose tampering can be detected (§9).
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
- Recording what happens *inside* a guest beyond what crosses an interface the
  platform controls (exec, files API, egress).

---

## 3. Concepts

| Term | Meaning |
|---|---|
| **Host** | A server that can run microVMs: Linux with KVM in production, macOS with Hypervisor.framework for development only. Runs the host agent. |
| **Pool** | A named group of hosts. `untrusted` and `services` are separate pools on separate hosts, so untrusted code never shares a host with production apps. |
| **Template** | An immutable root filesystem plus defaults (CPU, memory, policy, start command) for one CPU architecture (`arm64` or `x86_64`). Versioned by content hash; the scheduler only places it on hosts of its architecture. Built from an OCI image or Dockerfile from M3; until then, prebuilt and trusted. |
| **Snapshot** | A saved disk state, for rollback and branching. Memory snapshots for fast starts arrive with driver 2 (§4.3). |
| **Machine** | A running microVM from a template. Has an ID, a mode, a policy, a state. |
| **Mode** | `ephemeral` or `service`. Decides lifetime, restart behaviour, ingress and pool. |
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
│  secret vault · audit sealer · event log              │
└───────────────┬───────────────────────────┬──────────┘
                │ mTLS                      │ routes
                ▼                           ▼
┌──────────────────────────────┐   ┌───────────────────┐
│ host agent (one per host)    │   │ edge proxy        │
│  spawns one VMM process per  │   │  TLS, domains,    │
│  machine (sandboxed, no root)│   │  preview URLs     │
│  egress proxy · event spool  │   └─────────┬─────────┘
└───────────────┬──────────────┘             │
                │ vsock (host connects in)   │ tap (Linux)
                ▼                            ▼
        ┌────────────────────────────────────────┐
        │ microVM                                 │
        │  guest agent: exec, stdin, ports        │
        │  in-guest confinement (second wall)     │
        │  user workload (unprivileged uid)       │
        └────────────────────────────────────────┘
```

### 4.1 Control plane

One binary. State in SQLite (replication later if needed). Responsibilities:

- **API** — REST for commands, a streaming channel (WebSocket or NDJSON) for
  exec output and events.
- **Scheduler** — picks a host in the right pool with free capacity and a warm
  machine of the requested template. Bin-packing, nothing clever.
- **Template registry** — stores root filesystems by content hash and pushes
  them to hosts on demand.
- **Policy compiler** — merges the policy layers (§7.1) into one resolved
  policy per machine, and rejects any layer that tries to widen a parent.
- **Authorization** — API keys carry scopes. Creating in the `untrusted` pool,
  deploying a service, and promoting a machine to a service (§11.3) are three
  separate scopes. A key can only touch its own project's machines.
- **Secret vault** — encrypted at rest. Hands secrets only to egress proxies,
  never to machines. Pushes revocations to proxies (§8).
- **Event log** — every state change and its event are written in **one SQLite
  transaction**, with a per-machine sequence number owned by the control
  plane. Retention: 7 days for ephemeral machines, configurable for services.
- **Audit sealer** — receives audit records, chains and seals them (§9).

The control plane holds no machine traffic. If it goes down, running machines
keep running and new ones cannot be created. Host agents **spool** events and
exec output to local disk while the control plane is unreachable, then replay
them in order on reconnect; the control plane reconciles machine state against
each host's report.

### 4.2 Host agent

One binary per host, run as root under systemd, because it creates users,
namespaces and cgroups. It never runs guest code itself. Talks to the control
plane over mutual TLS.

- **One VMM process per machine.** libkrun's `krun_start_enter` takes over the
  calling process and does not return, so the host agent spawns a small
  `sandpiper-vmm` process for each machine and supervises it. Killing the
  process destroys the machine.
- **VMM processes are sandboxed.** Each runs as its own unprivileged user
  (never the operator account, which may have sudo), in its own mount, PID and
  network namespace on Linux, with a seccomp filter, and with only that
  machine's files visible. A VMM escape then lands in an empty, powerless
  process — this is the outer layer of the first wall.
- **Network:** on Linux, one tap device (libkrun's `krun_add_net_tap`) in its
  own network namespace per machine, wired only to the egress proxy (§7.3).
  A machine with `egress: 'none'` gets no network device at all. libkrun's implicit
  socket-forwarding networking (TSI, which lets the guest open sockets *as the
  host process*) is **disabled** on every production host.
- **Warm pool:** N booted, idle machines per popular template, one process
  each, ready to hand out. (Driver 1 cannot pause; see §4.3.)
- Runs the **egress proxy** for its machines (§7.3, §8).
- Enforces CPU, memory, disk and wall-clock limits from outside the guest
  (cgroups on Linux). Host cgroups see only VMM threads, not guest processes,
  so the policy's `pids` limit is enforced **inside** the guest by the guest
  agent (§4.4).
- Ships logs, metrics and audit records to the control plane, spooling when it
  cannot (§4.1).

### 4.3 Runtime driver

An interface, so the microVM engine can change without touching the rest.
Every driver implements the required operations; the optional ones are
capabilities a driver declares, and the scheduler only uses what is declared.

```
required:  create(template, resources) → vm · start(vm) · stop(vm)
optional:  pause(vm) · resume(vm)
           snapshot(vm, disk|memory) → snapshot · restore(snapshot) → vm
```

- **Driver 1 (M0–M4): libkrun**, pinned to a tested version (1.19.7 at the time
  of writing). Implements `create/start/stop` and, from M3, disk snapshots
  done by the host on a block image (see §10). No pause, no memory snapshots.
  The root filesystem is a virtio-fs directory in M0 and a block image from M1,
  so that copy-on-write snapshots are possible. The guest kernel comes from
  libkrunfw, which libkrun loads alongside itself.
- **Driver 2 (M5): Firecracker**, for memory snapshots, fast clones and a
  smaller attack surface. Linux only.

Every driver must give each machine its own guest kernel. Container-only
isolation is not an accepted driver for the `untrusted` pool.

### 4.4 Guest agent

A small static Rust binary baked into every template. Started by the guest's
init before any workload.

- **The host connects in; the guest never dials out.** At boot the guest agent
  binds its vsock port (one fixed port) before any workload exists, so no other
  process can take it. The host side of that port is a Unix socket the host
  agent created for this one machine, so the *connection itself* identifies the
  machine. Nothing the guest says about its own identity is trusted.
- **Workloads cannot reach vsock.** Every workload process runs as an
  unprivileged uid with no capabilities and a seccomp filter that refuses
  `socket(AF_VSOCK, …)`. Root inside the guest is reserved for the guest agent.
- **Guest messages are untrusted input.** The protocol is length-prefixed
  frames with a hard size cap (1 MiB per frame), parsed by a bounded decoder.
  The host agent exposes no management operation over this channel — the guest
  can only answer requests the host made.
- Operations: `exec` (run a command, stream stdout/stderr, accept stdin,
  return exit code), `ports` (report listening ports for preview). `pty` and a
  `files` API arrive later; until then files move through `exec` with stdin.
- Output is capped per exec (1 MiB per stream by default); the cap is marked
  in the result when hit.
- Applies the **in-guest confinement** part of the policy (§7.2) to every
  process it starts, including the `pids` limit through a guest cgroup.
- Installs the host's proxy CA (§8) and points common runtimes at it on every
  exec: `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`.
- Freezes and thaws guest filesystems on request, for snapshots (§10).

### 4.5 Edge proxy

Terminates TLS. Built on an off-the-shelf proxy configured by the control
plane through its admin API. Two kinds of route:

- **Preview URLs** for ephemeral machines, when policy allows exposure.
  - On a **separate registrable domain** used for nothing else (for example
    `sandpiper-preview.dev`), listed on the Public Suffix List so sibling
    subdomains cannot share cookies.
  - Hostnames are **random**, not machine IDs: `k3v9q2-8080.<preview-domain>`.
  - One **wildcard certificate** issued by DNS-01, never one certificate per
    preview (that would hit CA rate limits within minutes).
  - The first request carries a signed, short-lived token bound to machine,
    port and expiry. The proxy swaps it for a cookie scoped to that one
    hostname and redirects to strip the token from the URL. **Every** request
    and WebSocket upgrade is checked; destroying the machine revokes the route.
- **Service domains** (`app.example.com`). Certificates are issued on demand,
  but only for names the control plane has registered: the proxy's
  "may I issue for this name?" check calls the control plane and is refused for
  anything else.

---

## 5. Machine lifecycle

```
           create
             │
             ▼
         starting ──fail──► failed
             │
             ▼
          running ──── stop / TTL / policy violation ───┐
             │                                           │
   restore:  │ stop → swap disk → boot (never live)      ▼
             └──────────────────────────────────►  stopped ──► destroyed
                                                     (disk wiped, audit sealed)
```

`paused` exists only on drivers that declare `pause` (driver 2).

| | ephemeral | service |
|---|---|---|
| Default TTL | 15 min idle, 4 h max | none |
| On crash | stays `failed`, caller decides | restarted with backoff |
| On host loss | lost; caller sees `failed` | rescheduled from last disk snapshot |
| Ingress | none, or preview URL | domain + TLS |
| Disk | discarded on destroy | persistent volume |
| Pool | `untrusted` | `services` |

A **policy violation** that the host can observe (egress to a blocked domain
repeated past a threshold, a resource limit breached) stops the machine and
records why.

---

## 6. API sketch

All calls are authenticated with a project-scoped API key (§4.1). Machine IDs
are opaque.

```
# M1
POST   /v1/machines                  { template, policy?, ttl?, metadata? }
GET    /v1/machines/:id
DELETE /v1/machines/:id
POST   /v1/machines/:id/exec         { cmd, cwd?, env?, stdin?, policy? } → stream
GET    /v1/events?machine=&from=     replayable event stream

# M2–M3
POST   /v1/templates                 build from OCI ref or Dockerfile (runs in an untrusted machine)
GET    /v1/templates/:id
POST   /v1/machines/:id/snapshots    { kind: disk }
POST   /v1/machines/:id/restore      { snapshot }
GET    /v1/machines/:id/audit        records + seals

# M4–M5
POST   /v1/services                  { template, domains, env, volume, replicas: 1 }
POST   /v1/services/:id/deploy       { template }  → new machine, health check, swap
POST   /v1/machines/:id/expose       { port } → preview URL
GET    /v1/machines/:id/pty          upgrade to WebSocket
GET|PUT /v1/machines/:id/files?path=
```

Mode is not a free parameter: `POST /v1/machines` creates ephemeral machines in
`untrusted`; services are created only through `/v1/services`, which needs the
service scope.

**Events are durable and replayable.** Each event has a per-machine sequence
number; a client that disconnects reconnects with `from=<seq>` and misses
nothing within the retention window. This matches how Litro sessions already
work, so a sandbox job survives a browser refresh end to end. Slow consumers do
not block machines: the server keeps the log, the client catches up.

A TypeScript client (`@beatzball/sandpiper`) wraps this API. It is the only
thing Litro depends on.

---

## 7. Policy

A policy is **what a machine may touch**. It has five parts:

```ts
interface Policy {
  fs: {
    read:  string[];          // absolute path prefixes inside the guest
    write: string[];          // absolute path prefixes; write implies read
    deny:  string[];          // absolute path prefixes; wins over read/write
  };
  net: {
    egress: 'none' | 'allowlist';   // 'open' exists only in the services pool
    allow: NetRule[];               // see §7.1
    expose: boolean;                // may request preview URLs
  };
  secrets: GrantId[];         // references to immutable grants, see §8
  resources: { cpus: number; memoryMb: number; diskMb: number; pids: number };
  time: { idleMs: number; maxMs: number; execMaxMs: number };
}

interface NetRule {
  host: string;               // exact name, or "*.suffix" (one label or more)
  port?: number;              // default 443
  methods?: string[];         // HTTP only; default any
  pathPrefix?: string;        // HTTP only; default "/"
}
```

### 7.1 Layers, and narrowing only

Policies stack. Each layer can only **narrow** the one above it, never widen:

```
pool  →  template  →  machine  →  exec (one command)
```

- A pool sets the ceiling (the `untrusted` pool has no `egress: 'open'`).
- A template sets what its software needs (a Python template may write `/tmp`
  and `/work`).
- A machine narrows for one job.
- A single `exec` can narrow further for one command.

**The rule language is kept small so that "is A narrower than B?" is decidable
and simple.** No free-form globs, no regexes.

| Part | Child ⊆ parent when… |
|---|---|
| `fs.read` | every child prefix is equal to or under some parent `read` or `write` prefix, and not under a parent `deny` |
| `fs.write` | every child prefix is equal to or under some parent **`write`** prefix (never a read-only one), and not under a parent `deny` |
| `fs.deny` | the child's deny set contains every parent deny prefix (it may add more) |
| `net.egress` | `none` ⊆ `allowlist` |
| `net.allow` | every child rule is covered by one parent rule: same or narrower host (`a.x.com` ⊆ `*.x.com`), same port, method set ⊆, path prefix under |
| `net.expose` | `false` ⊆ `true` |
| `secrets` | child grant IDs ⊆ parent grant IDs. Grants are immutable: a child can drop a grant, never edit one |
| `resources`, `time` | each number ≤ parent |

An **omitted field inherits the parent's value**. Paths are normalised
(no `..`, no trailing `/`, symlinks resolved by the guest agent at apply time)
before comparison.

The compiler resolves the stack when the machine is created and when each exec
starts. A request that widens a parent is **rejected**, not silently clipped,
so the caller learns the rule.

Once applied inside a running process, a policy **cannot be loosened** by that
process or its children. Loosening requires a new exec from outside.

### 7.2 Two walls, and what each really stops

| Wall | Where | Stops |
|---|---|---|
| **microVM** (plus the sandboxed VMM process around it) | host | Anything reaching the host kernel, other machines, host files. **The trust boundary.** |
| **in-guest confinement** | guest agent | A process touching files, syscalls or sockets its exec policy does not allow (filesystem access rules, a syscall filter, no new privileges, an unprivileged uid). |

Be honest about the second wall: processes in one guest can still share state
— background processes left running, files in shared writable paths, the
machine's single network identity at the egress proxy. Per-exec narrowing is
**defence in depth, not a trust boundary.** Work at different trust levels runs
in **separate machines**. Egress is attributed to the machine, not to the exec.

### 7.3 Network (Linux hosts)

- Each machine's only route out is the host's **egress proxy**, keyed by the
  machine's tap interface, so one proxy per host serves all machines safely.
  No route to other machines, the host, the LAN or cloud metadata.
- **The proxy resolves names itself** and connects to the address it resolved.
  It refuses private, loopback, link-local and metadata ranges, so DNS
  rebinding and literal-IP requests go nowhere.
- Two TLS modes, chosen per rule:
  - **Intercepted** — required for any rule with `methods` or `pathPrefix`,
    and for secret grants. The proxy terminates TLS, requires the HTTP
    authority to equal the SNI and the rule's host, enforces method and path,
    and re-originates TLS, **verifying the upstream certificate**. Protocol
    upgrades and `CONNECT` are refused unless the rule allows them.
  - **Opaque** — host and port only. The proxy checks SNI and passes bytes
    through. It cannot see the HTTP authority inside, so on shared front ends
    (CDNs) a client can reach other sites behind the same address (domain
    fronting). Opaque rules are allowed only where that is acceptable, and the
    guarantee is stated as "connections to this SNI", nothing more.
- DNS from the guest is answered only by the proxy; blocked names get no
  answer. Other UDP, ICMP and IPv6 are dropped (IPv6 may be added later).
- `egress: 'none'` is enforced by the host firewall, not only by the proxy.
- Every allowed and denied connection is an audit record.

---

## 8. Secrets: usable, never handed over

Code in a machine needs to call paid APIs. It must not receive the key.

1. A **grant** is an immutable record with an ID. It names a secret and
   exactly where it may be used:
   ```ts
   {
     id: 'gr_4kd9',
     secret: 'ANTHROPIC_API_KEY',
     host: 'api.anthropic.com',
     methods: ['POST'],
     pathPrefix: '/v1/messages',
     inject: { header: 'x-api-key' },
   }
   ```
2. The machine receives a **placeholder** in its environment, for example
   `ANTHROPIC_API_KEY=sp_ph_7f3c…`. The placeholder is unique to this machine
   and grant, and worthless anywhere else.
3. HTTPS to granted hosts is intercepted by the egress proxy. The guest trusts
   the proxy's CA: **one CA per host**, whose public certificate the guest agent
   installs at boot. The CA's private key lives only in the proxy's memory —
   never in a template, a guest or a snapshot. Clients that pin certificates
   fail closed.
4. The proxy replaces the placeholder **only** when all of these hold:
   - the request matches the grant's host, method and path prefix;
   - the placeholder is the **entire value of the one configured header**.
   It never rewrites bodies, URLs, query strings or any other header.
5. An intercepted request that carries a placeholder anywhere else — another
   header, the body, the URL, or a request to a different host — is
   **rejected** (not forwarded, nothing rewritten) and logged as a probe.
   Opaque traffic cannot be inspected, so probe detection covers intercepted
   traffic only.
6. The proxy **does not follow redirects** on credentialed requests; the
   redirect goes back to the guest without the credential.
7. The real value lives in the control-plane vault and in proxy memory. It
   never enters the guest, its disk, its snapshots or its logs.
8. **Revocation:** the control plane pushes revocations to proxies
   immediately; proxies also drop cached secrets after 60 seconds, so a missed
   push still expires within a minute.

**Trust limit, stated plainly:** once the real key reaches the upstream, the
upstream decides what happens to it. An endpoint that echoes request headers
back would return the key to the guest. So grants name narrow, reviewed
endpoints (host + method + path), never "any request to this site", and
scoped, short-lived credentials are preferred over master keys.

---

## 9. Audit log

What is recorded: every **API action** on a machine (created, exec started,
exec finished with exit code and duration, snapshot, restore, destroy), every
**egress decision** (allowed, denied, secret injected — name and host, never
value — and placeholder probes), and every **policy violation**. What happens
inside the guest without crossing one of those interfaces is not recorded.

- Records are **append-only** and **hash-chained**: each carries the hash of
  the one before it.
- The chain is **sealed** every 1,000 records or 5 minutes, whichever comes
  first, for every machine, and once more at destroy. A seal signs: machine ID,
  sequence range, chain head hash, the previous seal's hash, and — for the
  final seal — the terminal status.
- Seals are **copied off the host** to the control plane as they are made, and
  optionally to an operator-chosen external store (none is required, per goal
  7). A truncated tail is then detectable against a checkpoint the host cannot
  rewrite.
- `GET /v1/machines/:id/audit` returns the records and seals. With the public
  key, a verifier can check that nothing **up to the latest seal it has
  independently obtained** was removed, reordered or edited. Records after
  that seal are not yet protected against tail deletion. A **destroyed**
  machine's log is complete only if it ends in a final seal with a terminal
  status.

This answers "what did the agent do through the platform?" after the fact,
with proof.

---

## 10. Snapshots and rollback

- **Before-exec snapshots.** An exec can ask for `snapshot: true`. The host
  asks the guest agent to `sync` and freeze the writable filesystems, waits
  for pending disk writes to drain, takes a copy-on-write copy of the block
  image (reflinks on XFS/btrfs), then thaws. A machine that cannot be frozen
  is stopped cleanly before the copy instead.
- **Restore is never live.** Restore = stop the machine → swap in the snapshot
  image → boot. Running processes and open files do not survive a restore.
- **What rollback cannot undo:** anything that left the machine — network
  calls, API side effects. Policy, grants and revocations live outside the
  machine and are not rolled back.
- **Branching.** Restoring a snapshot into a *new* machine forks the state.
- **Memory snapshots (driver 2).** Clones are made only from pristine template
  snapshots, never from a machine that has run user code. Each clone gets new
  identity: machine ID, placeholders, guest-agent session, and fresh entropy
  (VM generation ID); it re-installs its current host's proxy CA and
  re-establishes its vsock connection.

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
    const r = await m.exec(cmd);
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
parallel, then expose the result as a preview URL. Steps at different trust
levels use different machines (§7.2).

### 11.3 From sandbox to service (M4+)

A machine an agent built up can be snapshotted into a template and deployed as
a service. Promotion needs its own scope (§4.1), and the template is rebuilt
onto the `services` pool's hosts — the running untrusted machine itself never
moves.

---

## 12. Hosting

| Stage | Where | Isolation claim |
|---|---|---|
| M0 | **Apple Silicon Mac**, libkrun on Hypervisor.framework, arm64 Linux guests | **None. Development only. Never runs untrusted code.** libkrun on macOS uses TSI networking (guest sockets act as the host process), there are no network namespaces or taps, and the VMM runs as the logged-in user. |
| M1–M3 | Homelab Linux box with KVM | Full design (§7). `untrusted` pool only. |
| M4+ | Add rented bare-metal hosts (Hetzner dedicated, OVHcloud, Latitude.sh, Vultr Bare Metal, Scaleway Elastic Metal) | `services` gets its own host(s). |
| Only if needed | GCP or AWS with nested virtualization / `.metal` | Burst capacity; most expensive. |

Linux hosts need **KVM**: bare metal, or a cloud VM with nested
virtualization. Shared-vCPU cloud VMs without it cannot run this. Both x86_64
and arm64 Linux hosts work.

Until a second host exists, service mode runs only trusted test apps; no
production service shares a host with untrusted machines.

macOS binaries that use Hypervisor.framework must be code-signed with the
`com.apple.security.hypervisor` entitlement (an ad-hoc signature is enough
locally).

---

## 13. Milestones

| | Scope | Done when |
|---|---|---|
| **M0** | **On the Mac.** `sandpiper-vmm` boots an arm64 Linux rootfs with libkrun. The guest agent binds vsock; the host connects, sends one `exec`, gets stdout, stderr and exit code back; the machine is destroyed. Wire format fixed. | `hello` comes back over vsock in < 1 s from process start, repeatably. |
| **M1** | **On a Linux KVM box.** Host agent + thin control API, one host, one prebuilt **trusted** template on a block image, built for the box's architecture. `create/exec/destroy`, stdin, output caps, TTLs. Sandboxed VMM processes, cgroup limits, TSI off, **`egress: none` enforced by the host firewall**. Event log with replay. TS client. | Litro `run-code` works end to end in the playground; a machine cannot reach the LAN or internet. |
| **M2** | Policy language and compiler (§7.1), in-guest confinement, egress proxy with allow-list and DNS/IP rules (§7.3). | A suite of escape attempts all fail and all appear in the log. |
| **M3** | Secrets via placeholders (§8), audit chain + seals + off-host checkpoints (§9), disk snapshots and restore (§10). Template builds from OCI/Dockerfile, run inside untrusted machines. | An agent calls a paid API with a key it never receives; audit verifies offline. |
| **M4** | Service mode on a separate host: persistent volumes, edge proxy with registered domains, deploy with health-checked swap, restart on crash, promotion. | A Litro app is served on its own domain from this platform. |
| **M5** | Driver 2, memory snapshots, branching, preview URLs, PTY, files API, multi-host scheduling. | Start from snapshot < 300 ms; two hosts share load. |

---

## 14. Open questions

1. ~~**Language.**~~ **Decided: Rust** for the control plane, host agent, VMM
   wrapper and guest agent. libkrun is reached through its C API (a thin FFI
   layer); everything else is Rust. The client stays in TypeScript.
2. **Guest base images.** OCI images (familiar, reuses existing images) or a
   minimal fixed base plus packages (smaller, faster boot). Recommendation: OCI
   images converted to block images, with a slim default template.
3. **Egress TLS interception.** Needed for secret injection and path rules.
   Alternative for traffic that should not be intercepted: SNI-only
   allow-listing and no secret grants. Probably both, chosen per rule.
4. ~~**Where builds run.**~~ **Decided:** inside a machine in the `untrusted`
   pool, from M3. Until then templates are prebuilt and trusted.
5. **Auth model beyond one operator.** Project-scoped API keys with scopes are
   enough now. Revisit if others get access.
6. ~~**Name.**~~ **Decided: sandpiper.** Crate `sandpiper`, npm
   `@beatzball/sandpiper`, repo `beatzball/sandpiper`.
7. **Preview domain.** Needs a dedicated registrable domain (§4.5). Pick before
   M5.
