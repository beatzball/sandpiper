# sandpiper — design

Status: draft · revised 2026-10-23 after two review rounds · extended
2026-11-08 with ideas from a review of a similar tool

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
  deploying a service, promoting a machine to a service (§11.4), and using
  dev boxes (§11.5) are four separate scopes. A key can only touch its own project's machines.
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
- **Guest frames are decoded in the VMM process, not in the host agent.** The
  VMM process (unprivileged, sandboxed) parses the guest's frames and passes
  only checked, typed messages to the root host agent. A decoder bug then
  lands in the sandboxed process, not in root.
- **Network:** on Linux, one tap device (libkrun's `krun_add_net_tap`) in its
  own network namespace per machine, wired only to the egress proxy (§7.3).
  A machine with `egress: 'none'` gets no network device at all. libkrun's implicit
  socket-forwarding networking (TSI, which lets the guest open sockets *as the
  host process*) is **disabled** on every production host.
- **Warm pool:** N booted, idle machines per popular template, one process
  each, ready to hand out. (Driver 1 cannot pause; see §4.3.)
- Runs the **egress proxy** for its machines (§7.3, §8), as its own
  unprivileged user with a seccomp filter, never as root. The proxy holds
  every real key and CA key for the host, so it has per-machine connection
  and rate limits, and its parsers are fuzzed. The host agent opens the
  proxy's listening socket inside each machine's network namespace and passes
  it to the proxy, so the proxy needs no privilege to enter namespaces.
- Enforces CPU, memory, disk and wall-clock limits from outside the guest
  (cgroups on Linux). Host cgroups see only VMM threads, not guest processes,
  so the policy's `pids` limit is enforced **inside** the guest by the guest
  agent (§4.4).
- Ships logs, metrics and audit records to the control plane, spooling when it
  cannot (§4.1). Audit records that a guest can cause (denied requests,
  probes) are rate-limited per machine, and each machine has a spool quota. A
  machine that hits either limit is stopped as a policy violation (§5).
- Limits disk I/O per machine (cgroup `io.max`) as well as CPU and memory.
- **Checks the walls of every machine, and fails closed** (Linux production
  hosts; the Mac makes no claim, §12). A failed check stops the machine, marks
  it `failed` and records why. A broken host then fails loudly instead of
  quietly running open machines.
  - **Before any workload.** The guest agent runs the inside probes before it
    starts the template's start command or any exec. Nothing from the template
    runs until the checks pass.
  - **Inside probes, as the workload uid:** a direct connection to each of a
    set of **canary listeners** the host agent runs (outside the machine's
    namespace, on the host, on a LAN-facing address, and one inside the VMM
    process's own network namespace, which catches TSI), and a
    `socket(AF_VSOCK, …)` call, which must be refused. A probe fails if any
    connection **reaches a canary**, judged on the host side, never from what
    the guest reports. Blocked paths use reject rules, so probes return at
    once and do not cost boot time.
  - **Outside checks:** the machine's namespace, tap (if any), cgroup and
    TSI setting are the ones its policy asks for. sandpiper keeps its
    firewall rules in its own nftables table and compares a hash of it.
  - **Again at hand-out.** A warm machine is checked at boot, before its
    caller's policy is known. The policy-specific outside checks run again
    when it is handed out.
  - **On a timer.** The host agent re-checks its nftables table hash. If
    another program changed or flushed it, the host stops starting machines,
    takes down the network links of running machines, and raises an alert.
  - **Upgrades drain the host.** Proxy CA keys live only in memory (§8), so
    the host agent is upgraded only after its machines are moved or stopped.

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

A small static Rust binary in every template. Started by the guest's init
before any workload.

- **The platform adds the guest agent and init, not the template build.**
  From M3, templates are built in untrusted machines (§12). The guest agent,
  init and their config go on a read-only layer that the platform adds after
  the build, outside the build machine, and that the build's files cannot
  shadow. The template signature covers this layer. Otherwise every in-guest
  wall would be code the attacker wrote.
- **Fresh kernel entropy at every boot.** The host passes a random seed; the
  guest agent mixes it into the kernel's pool before any workload, so machines
  from one template or one disk branch never share kernel random state.
  Random state that programs saved on disk is not covered: templates must not
  ship persistent identities (SSH host keys, saved RNG seeds); they are made
  at first boot.

- **The host connects in; the guest never dials out.** At boot the guest agent
  binds its vsock port (one fixed port) before any workload exists, so no other
  process can take it. The host side of that port is a Unix socket the host
  agent created for this one machine, so the *connection itself* identifies the
  machine. Nothing the guest says about its own identity is trusted.
- **Workloads cannot reach vsock.** Every workload process runs as an
  unprivileged uid with no capabilities and a seccomp filter that refuses
  `socket(AF_VSOCK, …)`. Root inside the guest is reserved for the guest agent.
  The uid drop and this one seccomp rule come in M1; the configurable
  confinement of §7.2 comes in M2.
- **Guest messages are untrusted input.** The protocol is length-prefixed
  frames with a hard size cap (1 MiB per frame), parsed by a bounded decoder.
  The host agent exposes no management operation over this channel — the guest
  can only answer requests the host made.
- Operations: `exec` (run a command, stream stdout/stderr, accept stdin,
  return exit code), `ports` (report listening ports for preview). `pty` and a
  `files` API arrive later; until then files move through `exec` with stdin.
- Output is capped per exec (1 MiB per stream by default); the cap is marked
  in the result when hit. A **binary stream mode** with an explicit size limit
  set by the host carries file and git transfers (§11.3); a transfer that hits
  its limit or ends early is reported as incomplete, never as success.
- Applies the **in-guest confinement** part of the policy (§7.2) to every
  process it starts, including the `pids` limit through a guest cgroup.
- Installs the machine's proxy CA (§8) and points common runtimes at it on every
  exec: `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`.
- Freezes and thaws guest filesystems on request, for snapshots (§10).

### 4.5 Edge proxy

Terminates TLS. Built on an off-the-shelf proxy configured by the control
plane through its admin API. It rejects ambiguous requests (for example,
conflicting `Content-Length` and `Transfer-Encoding`) and normalises the rest
before it forwards them, so a request cannot mean one thing to the edge and
another to the machine behind it. Two kinds of route:

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

A **dev box** (§11.5) is a record that runs one `ephemeral` machine at a
time, in `untrusted`. Its disk is kept as a snapshot between machines, and it
has its own time limits.

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
# M2
POST   /v1/machines/:id/exec         { …, binary: { maxBytes } } → byte stream, complete: bool
POST   /v1/machines/:id/fetch        { ref } → { scratchRef }   (§11.3)
GET    /v1/events?machine=&from=     replayable event stream

# M2–M3
POST   /v1/templates                 build from OCI ref or Dockerfile (runs in an untrusted machine)
GET    /v1/templates/:id
POST   /v1/machines/:id/snapshots    { kind: disk }
POST   /v1/machines/:id/restore      { snapshot }
GET    /v1/machines/:id/audit        records + seals

# M3.5 (dev box, §11.5; needs the devbox scope)
POST   /v1/devboxes                  { template, preset, policy? }
GET    /v1/devboxes/:id
DELETE /v1/devboxes/:id              destroy the box and its snapshots
POST   /v1/devboxes/:id/start        new machine from the last snapshot
POST   /v1/devboxes/:id/stop         snapshot the disk, then destroy the machine
POST   /v1/devboxes/:id/code         { repo, ref } host fetches, copies a bundle in
GET    /v1/devboxes/:id/pty          upgrade to WebSocket

# M4–M5
POST   /v1/services                  { template, domains, env, volume, replicas: 1 }
POST   /v1/services/:id/deploy       { template }  → new machine, health check, swap
POST   /v1/machines/:id/expose       { port } → preview URL
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

Every attack and its result — blocked, kept inside, narrowed, or left to the
operator — is listed in `docs/threats.md`.

### 7.3 Network (Linux hosts)

- Each machine's only route out is the host's **egress proxy**, keyed by the
  machine's tap interface, so one proxy per host serves all machines safely.
  No route to other machines, the host, the LAN or cloud metadata.
- **The proxy resolves names itself** and connects to the address it resolved.
  It connects only to addresses that are **globally reachable** according to
  the IANA special-purpose address registries. This is an allow rule, not a
  list of refused ranges, so private, loopback, link-local, carrier-grade NAT,
  benchmark, reserved and metadata addresses are all refused, including when
  written inside IPv6 forms (IPv4-mapped, NAT64, 6to4, Teredo). DNS rebinding
  and literal-IP requests go nowhere.
- **The proxy connects upstream on IPv4 only** until IPv6 is designed.
- **An operator deny list** adds addresses that are public but still inside:
  at least the host's and the site's own public addresses, and the control
  plane's and edge proxy's addresses, which the control plane adds itself.
  Machines therefore cannot reach services served by this platform's own edge
  proxy. On a home network, a request to the
  site's own public IP can loop back through the router into the LAN (hairpin
  NAT), and the global-address rule cannot see that. A home WAN address
  changes, so the host agent looks up its own public address on a timer and
  updates the list; the wall check probes it.
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
- DNS from the guest is answered only by the proxy. The proxy checks a name
  against the machine's rules **before** it asks any upstream resolver;
  blocked names get no answer and are never sent upstream. Other UDP, ICMP and IPv6 are dropped (IPv6 may be added later).
- `egress: 'none'` means the machine gets no network device (§4.2). The host
  firewall is the second layer, for the case where a device is attached by
  mistake.
- Every allowed and denied connection is an audit record.
- **Starter presets.** The platform ships reviewed, named presets for common
  work: package registries, source hosts, model APIs. A machine must name a
  preset to use it; no pool turns one on by default, and the parent policy
  still limits it. Presets use intercepted rules only (no opaque CDN
  wildcards, so no domain fronting) and allow read methods and paths where
  the service allows it (for example, git clone over HTTPS needs
  `POST …/git-upload-pack`; `git-receive-pack` stays blocked). File storage
  and paste sites are not in any preset.
- **Blocked-hosts view.** `sandpiper net blocked <machine>` lists the denied
  requests from the audit records, grouped, with a count, the reason and the
  policy layer that denied each one. Where only the host was missing, it also
  shows the rule that would allow it, or "not allowed by the pool" if that
  rule would widen a parent. Host names come from the guest, so control
  characters are escaped before printing.
- **Limits, stated plainly:**
  - **Domain fronting** on opaque rules (above).
  - **Any allowed host that stores what it is sent is a way out.** Source
    hosts, registries and model APIs all accept data. An allowlist limits
    where data can go, not whether it can leave.
  - **Services that fetch for you.** An allowed API that fetches URLs on the
    caller's behalf (model APIs with web fetch or search tools, link
    previewers) can carry data to hosts that are not on the list. The guest
    writes the request body and the proxy does not change it, so the platform
    cannot turn these tools off per request. Only a provider-side setting on
    the key or account helps; grants for such APIs should use a key with those
    tools turned off where the provider offers one.

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
   the proxy's CA: **one CA per machine**, made by the proxy when the machine
   is handed out and its policy is resolved (for a warm machine, at hand-out,
   not at boot), whose public certificate the guest agent installs before any
   workload. The key is ECDSA P-256, so making one costs
   well under a millisecond. Each CA carries X.509 name constraints, so it can
   sign only the hosts that machine's rules intercept. A key that leaks alone
   (a crash dump, a log) then reaches one machine only; a compromised proxy
   still reaches every machine on its host. The CA's private key lives only in
   the proxy's memory — never in a template, a guest or a snapshot. Clients
   that pin certificates fail closed.
   - **Lifetime.** A CA lives as long as its machine. There is no in-place
     rotation: many runtimes read their CA file only at start, so a new CA
     under a running process breaks it. A service gets a new CA by a
     health-checked redeploy, which makes a new machine (§6).
   - **Proxy restart.** The keys are lost with the proxy's memory. Intercepted
     traffic from existing machines then fails closed. Ephemeral machines are
     replaced by their caller; services are redeployed automatically.
4. The proxy replaces the placeholder **only** when all of these hold:
   - the request matches the grant's host, method and path prefix;
   - the placeholder is the **entire value of the one configured header**
     (from M3.5, a grant may instead say `Bearer`: the value is then
     `Bearer ` and the placeholder, and nothing else).
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
9. **Credential-making calls.** Some calls turn one credential into another
   that works without the proxy: adding an SSH key, deploy key, CI runner or
   webhook on a code host, or minting a short-lived token on a model API.
   - **The main defence is the credential's own scope.** Creating a grant
     requires the operator to state that its credential cannot make
     credentials (a fine-grained token without key or hook administration).
     The platform cannot check that statement; if it is wrong, the guest can
     mint a credential through any operation the deny list misses and use it
     outside the proxy.
   - **A reviewed deny list is the second layer.** It is internal, not part of
     the policy language, so its patterns may use segment wildcards
     (`/repos/*/*/keys`). A grant whose host, method and path cover a listed
     endpoint is rejected when it is created. At run time the proxy refuses to
     inject a credential into a request to a listed endpoint.
   - **Single-endpoint APIs** (GraphQL, batch) carry every operation through
     one path, so a path list cannot see them. A grant that covers one is
     rejected. (Checking request bodies per API is on the §14.1 list.)
   - The list is best effort and is reviewed as providers add endpoints.

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
  The new machine gets new placeholders; old ones copied in its files are
  bound to the old machine (§8), so the proxy refuses them.
- **Memory snapshots (driver 2).** Clones are made only from pristine template
  snapshots, never from a machine that has run user code. Each clone gets new
  identity: machine ID, placeholders, guest-agent session, and fresh entropy
  (VM generation ID); it gets its own new proxy CA (§8) and re-establishes its
  vsock connection.

Snapshots are bound by the same policy as their machine and are deleted with
it unless pinned, or owned by a dev box (§11.5), which keeps its last one.

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

### 11.3 Getting work out

A machine mounts no host folder, so work comes out as **git objects**, not
through a shared directory. A hostile machine controls every byte it sends, so
the host side is built to parse it safely.

- The guest runs `git bundle create` for the requested ref. The host pulls the
  bundle through the binary stream mode of `exec` (§4.4), with an explicit
  size limit. A transfer that is cut short is rejected.
- The host verifies the bundle and fetches it in a **jailed process**: an
  unprivileged user, no network, no host secrets, CPU, memory, time and size
  caps, into a fresh bare scratch repository with no shared object store. It
  uses one explicit refspec into `refs/sandpiper/<machine>/<ref>`, with
  `fetch.fsckObjects` and `transfer.fsckObjects` on.
- Fetched work is then checked out or merged inside a machine, never in an
  operator's working checkout. Nothing on the host changes until a person or a
  trusted step merges it.
- The client wraps this as `m.fetch(ref)`. It returns the name of the fetched
  ref in the host's scratch repository, so a Litro tool can hand that branch to
  a review step.

### 11.4 From sandbox to service (M4+)

A machine an agent built up can be snapshotted into a template and deployed as
a service. Promotion needs its own scope (§4.1), and the template is rebuilt
onto the `services` pool's hosts — the running untrusted machine itself never
moves.

### 11.5 Dev box (M3.5)

A **dev box** is a place a person works in by hand, through a terminal, for
days. The main use: run a coding agent such as Claude Code in it, with the
operator's API key usable but never inside the box.

A box is a record that outlives its machines. It runs **at most one machine at
a time**; `start` refuses while one runs. Every `start` boots a **new**
machine from the box's last disk snapshot (§10), so each session gets a new
machine ID, new placeholders (§8) and a new proxy CA. Nothing that binds to a
machine is carried over.

**Lifecycle.**

```
created ──start──► running ──stop──► stopped ──start──► running …
                      │                  │
                      └──── max age ─────┴──► destroyed (snapshots deleted)
```

- **Every routine host-made stop snapshots first:** idle TTL, the machine's
  max time, a host drain. Three cases skip the snapshot and fall back to the
  last good one: a crash (the disk may be half written), a failed wall check
  and a policy violation (in both, the session is suspect). A snapshot that
  fails also falls back. Each case is an event.
- **Host loss** marks the box `failed`, with an event. `start` is refused;
  `DELETE` cleans up.
- **Time.** Three limits, all enforced by the host:
  - *Idle TTL* pauses while a terminal is attached: a PTY WebSocket that
    answered a ping in the last 60 s. Several terminals may attach at once.
  - *Machine max time* (box policy, default 12 h, within the pool's ceiling)
    ends one session with a snapshot.
  - *Box max age* (default 7 days) counts from creation, stopped time
    included. A warning event and a terminal notice come 24 h before. Then the
    box and its snapshots are destroyed. Work not taken out is lost.
- **Snapshots.** A box keeps only its last snapshot. Box snapshots cannot
  be pinned, count toward the project's disk quota, and are deleted with the
  box. Host loss loses them: they are not copied off the host.
- **Access.** Boxes need their own `devbox` scope (§4.1). It covers every
  operation on a box, on a box's machine and on a box's snapshot, including
  the general machine routes (`exec`, `fetch`, `snapshots`, `restore`,
  `DELETE`). A key without it, such as the one Litro agents use, cannot
  reach a box by any route.

**Parts.**

- **Terminal.** `GET /v1/devboxes/:id/pty` and `sandpiper shell <box>`. The
  PTY moves forward from M5 for this. The guest agent runs the shell as the
  workload user, like any `exec`. A PTY carries the guest's raw bytes to a
  person's terminal, so the PTY endpoint itself drops the sequences that read
  from or act outside that terminal: clipboard (OSC 52), hyperlinks (OSC 8),
  title and color queries, device control strings (DCS) and other report
  requests. Every client gets this filter. The rest passes, so other terminal
  tricks stay possible (`docs/threats.md` §5).
- **Code in.** The host fetches the repository and copies a git bundle into
  the box through exec stdin. The box gets no git-host token.
  - A box may name only repositories on its project's list.
  - The credential comes from the vault (§8), never from the request.
  - The fetch runs in a jailed helper, as in §11.3: an unprivileged user,
    network to the one git host only, CPU, memory, time and size caps, a
    fresh bare repository, no checkout, `fetch.fsckObjects` on.
- **Work out.** As git objects only (§11.3). Nothing on the host changes until
  a person merges it.
- **Agent presets.** One preset per agent. A preset lists every host the
  agent needs and the grants it uses; any other host is denied, so the agent
  must cope with denials. The agent gets a placeholder key in its environment
  at every session (§8). Settings saved on the disk must not hold a key: a
  placeholder saved there is dead in the next session. The proxy knows a
  box's earlier placeholders until the box is destroyed; one of them is
  refused and logged as stale, not counted as a probe (§8.5). Where an agent
  asks a person to approve a new key, the preset writes that approval into
  the agent's settings at each start.
  - A preset works only when the key is the entire value of one header, or
    a `Bearer` token in `Authorization`. The `Bearer` form is new proxy work
    for M3.5 (§8.4).
  - An agent that swaps its key for a second token is out of scope. The
    second token would reach the box. It waits for the host-owned login flow
    on the later list (§14.1).
  - Model APIs with server-side web tools can carry data anywhere (§7.3). A
    preset's key should have them off where the provider allows.
- **CA.** The guest agent writes the machine's proxy CA at every boot, over
  any copy on the disk. Settings that embed an older CA file fail closed.
- **Audit.** Every machine's records carry its box ID, so one query shows a
  box's whole history (§9).

**Subscription logins are not part of the dev box.** They stay on the later
list (§14.1), with their risk. A dev box uses API keys until a host-owned login
flow exists.

**What stays true.** A dev box is still untrusted: the agent in it, the code
it writes and the packages it installs are assumed hostile, as in
`docs/threats.md`. A box lives longer, so anything planted in it also lives
longer, most likely in the agent's own settings (hooks, tools, MCP servers),
which the next session runs with that session's grants. The max age is the
limit on that.

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

**Hardening for `untrusted` hosts.** A patched, pinned host kernel and
libkrun; current CPU microcode; all CPU side-channel mitigations on; SMT
(hyper-threads) off. The egress proxy and its keys still share the CPUs with
untrusted machines, so side channels are narrowed, not closed. Moving the
proxy to its own host is a later option.

macOS binaries that use Hypervisor.framework must be code-signed with the
`com.apple.security.hypervisor` entitlement (an ad-hoc signature is enough
locally).

**Signed releases and templates.** Two keys, for two jobs:

- **Release key** (offline, minisign) signs host binaries. Its public key is
  pinned on each host at install time (a step `docs/host-setup.md` gains with
  the first release), from a source
  checked independently of the release, because a key that ships beside the
  files it signs proves nothing if the whole release is replaced. Binaries are
  verified before they are installed.
- **Template key** (online, on the control plane) signs templates the platform
  builds or approves. Its public key is pinned on each host at enrolment. The
  host agent checks a template's signature and content hash before it boots
  it. The signature means "this control plane approved these bytes", so a
  changed registry store or host disk cannot swap a template. It does not mean
  the template's software is safe: from M3 it is built in an untrusted
  machine.

---

## 13. Milestones

| | Scope | Done when |
|---|---|---|
| **M0** | **On the Mac.** `sandpiper-vmm` boots an arm64 Linux rootfs with libkrun. The guest agent binds vsock; the host connects, sends one `exec`, gets stdout, stderr and exit code back; the machine is destroyed. Wire format fixed. | `hello` comes back over vsock in < 1 s from process start, repeatably. |
| **M1** | **On a Linux KVM box.** Host agent + thin control API, one host, one prebuilt **trusted** template on a block image, built for the box's architecture. `create/exec/destroy`, stdin, output caps, TTLs. Sandboxed VMM processes, cgroup limits, TSI off, **`egress: none`: no network device, host firewall as second layer** (§7.3), workloads as an unprivileged uid with `AF_VSOCK` refused, wall checks with canaries (§4.2). Fuzzed guest frame decoder in the VMM process, fresh entropy seed per boot, hardened host (§12). Event log with replay. TS client. | Litro `run-code` works end to end in the playground; a machine cannot reach the LAN or internet; a VMM started with TSI on, or with a network device on an `egress: none` machine, is marked `failed` by the wall check. |
| **M2** | Policy language and compiler (§7.1), in-guest confinement, egress proxy with allow-list, DNS/IP rules and a CA per machine for intercepted rules, starter presets and blocked-hosts view (§7.3, §8). Wall check adds proxy probes: a denied name gets no answer, a literal IP and each refused range are refused, a direct connection that skips the proxy is dropped. Fuzzed proxy parsers. Work out as git objects (§11.3). Signed host releases (§12). | A test per M1 and M2 row of `docs/threats.md` shows its stated result: **Blocked** attacks fail, **Kept inside** attacks affect nothing outside their machine, **Narrowed** attacks stay within their stated limit, and every attempt appears in the log. |
| **M3** | Secrets via placeholders and the credential-call deny list (§8), audit chain + seals + off-host checkpoints (§9), disk snapshots and restore (§10). Template builds from OCI/Dockerfile, run inside untrusted machines; signed templates (§12). | An agent calls a paid API with a key it never receives; audit verifies offline. |
| **M3.5** | **Dev box** (§11.5): box lifecycle and time limits, PTY and `sandpiper shell` with the terminal filter, stop and start through disk snapshots, code in as a bundle, `devbox` scope, `Bearer` grants, presets for coding agents. | A scripted PTY session brings code in, runs Claude Code with a placeholder key, stops, starts, and runs it again; a second `start` while running is refused; a search of the box's snapshot finds no real key; the proxy log shows the key injected and no probes; max age destroys the box; each M3.5 row of `docs/threats.md` has a test. |
| **M4** | Service mode on a separate host: persistent volumes, edge proxy with registered domains, deploy with health-checked swap, restart on crash, promotion. | A Litro app is served on its own domain from this platform. |
| **M5** | Driver 2, memory snapshots, branching, preview URLs, files API, multi-host scheduling. | Start from snapshot < 300 ms; two hosts share load. |

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
8. **Agent status out of a machine.** A coding agent running in a machine
   should report status words (`working`, `blocked`, `done`) to a coordinator
   on the host, such as a terminal agent view. The guest never dials out
   (§4.4), so the host must pull. Options: a long-running `exec` that streams
   one status line per change, or a guest-agent `events` operation the host
   subscribes to. Either way the words are untrusted input from a small fixed
   set. Not planned for a milestone yet.

### 14.1 Later: considered, not planned

- **Subscription logins, only with a host-owned login flow.** Let an agent in
  a machine use the operator's subscription login, not only an API key. The
  simple form, where the guest runs the login and the proxy catches the token
  on its way out, is **rejected**: the login response would hand tokens to the
  guest, against §8. A later version would need the host to run the login and
  the refresh itself, so the guest only ever sees placeholders. Risk either
  way: these login requests are not documented by the providers, any client
  release can break them, and the providers' terms may not allow it. A dev
  box (§11.5) would gain most from this; until then it uses API keys.
- **Agent config in, changes out after a diff.** Copy the operator's agent
  settings into a machine with secrets replaced by placeholders, and let
  changes back out only after a shown diff (hidden characters shown as codes)
  that a person approves.
- **Request-body checks per API**, so a grant could cover a single-endpoint
  API (GraphQL, batch) and allow only named operations (§8).
- **Script-style CLI tests.** Small text files that hold a command, its input
  and the expected output, for the host agent's CLI. (Fuzzing of parsers that
  read guest input is not "later": it ships with each parser, see §13.)
- **A shared signed package cache.** Only the host fills it, with signed
  packages, to make warm templates cheaper. State the leak: cache timing tells
  one machine what another machine fetched, so it is not for pools whose
  machines must not learn about each other.
