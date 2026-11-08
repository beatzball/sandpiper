# sandpiper — threats

What an attacker inside a machine can try, and what happens. Each attack is
sorted by result:

| Result | Meaning |
|---|---|
| **Blocked** | The attack does not work. A wall stops it. |
| **Kept inside** | The attack works, but only inside the one machine, which is thrown away. |
| **Narrowed, not closed** | The attack still works, but on a smaller scale or needs a further bug. The limit is stated. |
| **Left to you** | The platform cannot stop it. The operator must decide. |

**Who is the attacker.** Code running in an `ephemeral` machine in the
`untrusted` pool: a coding agent, the code it writes, or a package it
installs. Assume it is hostile and knows this document.

**Where the claims hold.** Only on a Linux KVM host (DESIGN.md §12). The macOS
development host makes no claim and must never run untrusted code.

**When a claim starts.** Most walls are designed, not built. The `From`
column says which milestone makes the claim true. Until then, the claim does
not hold. The M2 escape suite has a test for every row marked M1 or M2:
**Blocked** attacks must fail, **Kept inside** attacks must affect nothing
outside their machine, **Narrowed** attacks must stay within their stated
limit, and **Left to you** rows get no test. Later
rows get tests in their own milestone.

**"Blocked" means against the stated attack, with the walls working.** No
row claims that the microVM, KVM or a parser is free of bugs. Rows that
depend on such a bug say **Narrowed, not closed**.

Section numbers (§) point to `DESIGN.md`.

---

## 1. Escaping the machine

| Attack | Result | How | From |
|---|---|---|---|
| Use a bug in KVM to escape the microVM | **Narrowed, not closed** | Each machine has its own guest kernel, so there is no shared kernel to attack (§4.3). KVM is part of the host kernel, so a KVM bug can give the host directly; the VMM sandbox does not help. The host kernel is pinned and patched, and `untrusted` hosts run nothing else of value (§12). | M1 |
| Use a bug in the VMM's virtual devices (user space) to escape | **Narrowed, not closed** | The escape lands in the VMM process: its own unprivileged user, its own namespaces, a seccomp filter, and only that machine's files (§4.2). A further bug, in the host kernel, is needed. libkrun is pinned and patched (§12). | M1 |
| Send hostile frames over vsock to the host | **Narrowed, not closed** | The host connects in. Frames are size-capped and decoded by a fuzzed decoder in the unprivileged VMM process, not in the root host agent, which gets only typed messages (§4.2, §4.4). | M1 |
| Get root in the guest through a guest kernel bug | **Kept inside** | The VM is the trust boundary. In-guest walls (uid drop, `AF_VSOCK` refusal, `pids` limit, per-exec confinement) are defence in depth and fall to this (§7.2). Exec results and `ports` replies are then written by the attacker. Host-side walls and the host-judged canaries still hold (§4.2). | M1 |
| Open a vsock socket from a workload | **Blocked** (defence in depth) | Workloads run as an unprivileged uid with `socket(AF_VSOCK, …)` refused, checked at every start (§4.4). A guest-kernel root (row above) gets past this; vsock then reaches only the host's typed request channel. | M1 |
| Lie about the machine's identity to the host | **Blocked** | The connection itself names the machine. Nothing the guest says about its identity is trusted (§4.4). | M1 |
| Replace the guest agent or init through a template build | **Blocked** | The platform adds the guest agent and init on a read-only layer after the build, and the template signature covers it (§4.4, §12). | M3 |
| Use up CPU, memory, disk space or disk I/O | **Narrowed, not closed** | cgroup limits on CPU, memory and I/O, and a disk size per machine (§4.2). Neighbours on the same host can still slow down. | M1 |
| Fork-bomb inside the guest | **Kept inside** | The `pids` limit is enforced in the guest (§4.2). A guest-kernel root can lift it, but then only that machine's CPU and memory, which cgroups cap, are spent. | M2 |
| Keep a machine alive forever | **Blocked** | Idle and max TTL (§5) and an exec timeout (`execMaxMs`, §7) are enforced by the host. Guest clock changes do not affect them. | M1 |
| Flood the host with output, events or audit records | **Narrowed, not closed** | Output is capped per exec, frames at 1 MiB (§4.4). Guest-caused audit records are rate-limited and each machine has a spool quota; hitting either stops the machine (§4.2). | M1 |
| Predict another machine's random numbers | **Narrowed, not closed** | The host passes a fresh seed into the kernel's pool at every boot, and memory clones get a new VM generation ID (§4.4, §10). Random state a program saved on disk is copied into every branch; templates ship no persistent identities (§4.4). | M1 |

## 2. Reaching other places on the network

The rows below are about **direct connections**. Indirect ways out through
allowed hosts are listed separately and are not closed.

| Attack | Result | How | From |
|---|---|---|---|
| Connect anywhere from an `egress: none` machine | **Blocked** | The machine gets no network device. The host firewall is the second layer. TSI is off. A wall check at every start fails the machine if any of this is wrong (§4.2, §7.3). | M1 |
| Reach the LAN, the host or another machine | **Blocked**, except the hairpin path below | The only route out is the egress proxy, which connects only to globally reachable addresses (§7.3). | M2 |
| Reach cloud metadata (`169.254.169.254` and similar) | **Blocked** | Not a globally reachable address (§7.3). | M2 |
| DNS rebinding: a name that resolves to a private address | **Blocked** | The proxy resolves names itself and checks the address it connects to (§7.3). | M2 |
| Hide a private address inside an IPv6 form (NAT64, IPv4-mapped, 6to4, Teredo) | **Blocked** | The global-address rule covers these forms. The proxy connects upstream on IPv4 only (§7.3). | M2 |
| Reach the LAN through the site's own public IP (hairpin NAT on a home router) | **Narrowed, not closed** | The host agent keeps its own public address on the deny list, updated on a timer, and the wall check probes it (§7.3). Between an address change and the next update, the path may be open. | M2 |
| Reach the platform's own API or edge proxy | **Blocked** | The control plane puts their addresses on the deny list itself (§7.3). | M2 |
| Connect directly to a host that is not on the allowlist | **Blocked** | The proxy refuses it and records it (§7.3). | M2 |
| Exploit or exhaust the egress proxy | **Narrowed, not closed** | The proxy runs unprivileged with a seccomp filter, fuzzed parsers and per-machine connection and rate limits (§4.2). A proxy compromise reaches every machine and every key on its host (§8). | M2 |
| Domain fronting: an allowed name in SNI, another site behind the same CDN | **Narrowed, not closed** | Intercepted rules check that the HTTP authority equals SNI and the rule's host. Opaque rules cannot; their claim is only "connections to this SNI" (§7.3). | M2 |
| Send data out through an allowed host that stores it (a gist, a package publish, a model API upload) | **Narrowed, not closed** | Presets allow read methods and paths where the service allows it. Any allowed host that accepts data is still a way out (§7.3). | M2 |
| Encode data in DNS names under a wildcard rule (`*.x.com`) | **Narrowed, not closed** | The proxy checks a name against the rules before it sends it to any resolver (§7.3), so only names under allowed zones leave. Upstream resolvers and that zone's servers see them. Avoid wildcards where an exact name works. | M2 |
| Ask an allowed API to fetch a URL for you (model web fetch or search tools) | **Left to you** | The platform cannot turn these tools off per request. Use a key with them turned off where the provider offers one (§7.3). | — |
| Talk over UDP, ICMP or IPv6 from the guest | **Blocked** | Dropped. DNS is answered only by the proxy (§7.3). | M2 |

## 3. Stealing or misusing credentials

| Attack | Result | How | From |
|---|---|---|---|
| Read a platform-held real key from the environment, disk or memory | **Blocked** | The guest holds only a placeholder. The real value lives in the vault and in proxy memory (§8). | M3 |
| Send the placeholder to a host the attacker controls | **Blocked** for the real key | The placeholder is worthless off-platform. On intercepted traffic the request is also rejected and logged as a probe; on opaque rules it passes, but carries no key (§8). | M3 |
| Use the granted key for calls outside the grant | **Blocked** | Method and path are checked on intercepted traffic (§8). | M3 |
| Make the upstream echo the key back (an endpoint that returns request headers) | **Narrowed, not closed** | Grants name narrow, reviewed endpoints. Once the key reaches the upstream, the upstream decides (§8, trust limit). | M3 |
| Follow a redirect to carry the key to another host | **Blocked** | The proxy does not follow redirects on credentialed requests (§8). | M3 |
| Mint a new credential that works without the proxy (add an SSH key, a deploy key, a webhook; mint a short-lived token) | **Narrowed, not closed** | The operator must state the credential cannot mint credentials. A deny list of known minting endpoints is the second layer. Single-endpoint APIs (GraphQL, batch) cannot be granted (§8.9). | M3 |
| Reuse an old machine's placeholder found in a restored disk branch | **Blocked** | Placeholders are bound to their machine and grant; a branch gets new ones (§8, §10). | M3 |
| Steal the proxy's CA key to forge certificates for other machines | **Narrowed, not closed** | One CA per machine, name-constrained to that machine's hosts, held only in proxy memory. A compromised proxy still reaches every machine on its host (§8). | M2 |
| Read keys from the proxy through a CPU side channel | **Narrowed, not closed** | CPU mitigations on, SMT off, microcode current on `untrusted` hosts (§12). The proxy still shares CPUs with untrusted machines. | M2 |
| Keep using a key after it is revoked | **Narrowed, not closed** | Revocations are pushed at once; cached secrets expire within 60 seconds (§8). | M3 |
| Pin certificates to detect the proxy and refuse to work | **Kept inside** | Pinned clients fail closed. Nothing leaks (§8). | M2 |
| Steal a control-plane API key that a caller put into a machine | **Left to you** | Placeholders protect only grants (§8), not the platform's own API keys. Never pass an API key into a machine. Scopes limit what a stolen key can do (§4.1). | — |

## 4. Hiding what was done

| Attack | Result | How | From |
|---|---|---|---|
| Edit, remove or reorder audit records | **Narrowed, not closed** | Records are hash-chained and sealed, and seals are copied off the host (§9). Tampering is **detected** up to the latest seal a verifier holds; it is not prevented. | M3 |
| Delete the tail of the log after the last seal | **Narrowed, not closed** | Records after the latest independently held seal are not protected. A destroyed machine's log is complete only if it ends in a final seal (§9). | M3 |
| Undo what the machine did by rolling it back | **Left to you** | Rollback restores the disk only. Network calls, API side effects and credentials minted outside the proxy are not undone (§10). | M3 |
| Do things inside the guest that cross no platform interface | **Left to you** | The audit log records API actions, egress decisions and policy violations, not what happens inside the guest (§2, §9). Use separate machines for separate trust levels. | — |

## 5. Attacking the operator through results

| Attack | Result | How | From |
|---|---|---|---|
| Plant a hook, filter or config in a repo that runs on the host when work is fetched | **Blocked** | Work leaves as a git bundle. The host fetches it in a jailed process into a fresh scratch repo, with `fsckObjects` on and one explicit refspec (§11.3). | M2 |
| Send a hostile pack to exploit or exhaust the host's git | **Narrowed, not closed** | The fetch process is unprivileged, with no network, no host secrets, and CPU, memory, time and size caps (§11.3). A jail escape needs a further bug. | M2 |
| Plant symlinks, submodules or `.gitattributes` filters that act on checkout | **Kept inside** | Fetched work is checked out or merged inside a machine, never in an operator checkout (§11.3). | M2 |
| Put a backdoor, a changed CI config or an install script in the work itself | **Left to you** | The platform moves the work safely; it cannot judge it. Review before merge, and run CI for that branch in a machine, not on a trusted runner. | — |
| Write instructions in exec output to steer the calling agent | **Left to you** | Exec output goes back to the caller's model (§11.1). Treat it as untrusted input. The caller's API key should carry no service or promotion scope (§4.1). | — |
| Print terminal control codes in error messages or console output | **Blocked** | The VMM wrapper cuts and escapes guest text before it prints it (M0 `sanitize`). Blocked host names are escaped too (§7.3, M2). | M0 |
| Print terminal control codes in a command's stdout or stderr | **Left to you** | Command output is the program's own bytes and passes through unchanged. Escape it before showing it in a terminal or a web page. | — |
| Ship malicious code in a template that a build produced | **Left to you** | A template signature means "this control plane approved these bytes", not "this software is safe". Builds run in untrusted machines (§12). | — |
| Install a malicious package during a run | **Narrowed, not closed** | It runs in the machine like any other code, so the walls hold. It can still use the machine's grants and send data through its allowed hosts. | M1 |
| Swap a template on the registry or host disk | **Blocked** | The host agent checks the template's signature and content hash before boot (§12). | M3 |
| Swap a host binary in a release | **Blocked** | Binaries are verified against a release key pinned at install (§12). | M2 |
| Keep sensitive data in a pinned snapshot after the machine is gone | **Left to you** | Pinned snapshots outlive their machine (§10). They hold no platform keys, but may hold the work's own data. Unpin what is not needed. | M3 |

## 6. Machines attacking each other, and production

| Attack | Result | How | From |
|---|---|---|---|
| One machine reaches another over the network | **Blocked** | No route between machines (§7.3). | M1 |
| Read another machine's data through a CPU side channel | **Narrowed, not closed** | CPU mitigations on, SMT off, microcode current (§12). Machines still share hardware. | M1 |
| A process in one exec spies on another exec in the same machine | **Narrowed, not closed** | Per-exec confinement is defence in depth, not a trust boundary. Work at different trust levels runs in separate machines (§7.2). | M2 |
| Untrusted code shares a host with a production service | **Blocked** | `untrusted` and `services` pools are on separate hosts (§3, §12). | M4 |
| Get code an untrusted agent built promoted to a service | **Left to you** | Promotion needs its own scope, and the template is rebuilt for the `services` pool (§4.1, §11.4). A person decides what is promoted. | M4 |
| Call a production service's public endpoint from a machine | **Blocked** for this platform's services; **Left to you** for others | The edge proxy's addresses are on the deny list (§7.3). A service hosted elsewhere can be called like any allowed host; keep it off allowlists unless needed. | M2 |

## 7. Serving to the outside

| Attack | Result | How | From |
|---|---|---|---|
| Steal or replay a preview token | **Narrowed, not closed** | The token is short-lived, bound to machine, port and expiry, and swapped for a one-host cookie by a redirect (§4.5). A token stolen before use still works until it expires. | M5 |
| Use one preview's cookies on another preview | **Blocked** | The preview domain is on the Public Suffix List and cookies are scoped to one hostname (§4.5). | M5 |
| Reach a preview after its machine is gone | **Blocked** | Every request and WebSocket upgrade is checked; destroying the machine revokes the route (§4.5). | M5 |
| Serve phishing or malware from the preview domain | **Narrowed, not closed** | Previews need `expose` and use random hostnames on a domain used for nothing else (§4.5). The whole domain can still land on browser blocklists. The operator needs a takedown path. | M5 |
| Exploit the edge proxy with hostile responses or request smuggling | **Narrowed, not closed** | The edge proxy rejects ambiguous requests and normalises the rest (§4.5). A bug there is outside the microVM wall. | M4 |

## 8. Host failures

| Failure | Result | How | From |
|---|---|---|---|
| Another program flushes or changes sandpiper's firewall rules | **Narrowed, not closed** | A timer checks the nftables table hash. On a change, the host stops new starts and takes down running machines' links (§4.2). Machines may be open for up to one timer interval. | M1 |
| A VMM is started with TSI on, or with a network device on an `egress: none` machine | **Blocked** | The wall check marks the machine `failed`, using a canary inside the VMM's own namespace (§4.2). | M1 |
| The proxy restarts and loses its CA keys | **Blocked** (fails closed) | Intercepted traffic from existing machines fails. Ephemeral machines are replaced by their caller; services (from M4) are redeployed (§8). | M2 |

---

## Keeping this page true

- A change to `DESIGN.md` that adds, removes or weakens a wall changes this
  page in the same pull request.
- A row marked **Blocked** needs a test in the escape suite by its milestone.
  A row with no test says so.
- A new attack goes in the table that fits, with its result, even if the
  result is **Left to you**.
