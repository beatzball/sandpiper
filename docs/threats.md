# sandpiper — threats

What an attacker inside a machine can try, and what happens. Each attack is
sorted by result:

| Result | Meaning |
|---|---|
| **Blocked** | The attack does not work. A wall stops it. |
| **Kept inside** | The attack works, but only inside the one machine, which is thrown away. |
| **Narrowed, not closed** | The attack still works, but on a smaller scale. The limit is stated. |
| **Left to you** | The platform cannot stop it. The operator must decide. |

**Who is the attacker.** Code running in an `ephemeral` machine in the
`untrusted` pool: a coding agent, the code it writes, or a package it
installs. Assume it is hostile and knows this document.

**Where the claims hold.** Only on a Linux KVM host (DESIGN.md §12). The macOS
development host makes no claim and must never run untrusted code.

**When a claim starts.** Most walls are designed, not built. The `From`
column says which milestone makes the claim true. Until then, the claim does
not hold. The M2 escape suite tests every row marked M1 or M2; later rows get
tests in their own milestone.

Section numbers (§) point to `DESIGN.md`.

---

## 1. Escaping the machine

| Attack | Result | How | From |
|---|---|---|---|
| Break out of the guest kernel into the host kernel | **Blocked** | Each machine has its own guest kernel in a microVM. No container-only driver is accepted (§4.3). | M1 |
| Exploit a bug in the VMM to run code on the host | **Narrowed, not closed** | A VMM escape lands in an unprivileged user with its own namespaces, a seccomp filter and only that machine's files (§4.2). A second bug, in the host kernel, is then needed. | M1 |
| Reach the host agent through vsock | **Blocked** | The host connects in; the guest never dials out. The host agent offers no management operation on that channel, and frames are size-capped and parsed by a fuzzed decoder (§4.4). | M1 |
| Open a vsock socket from a workload to talk to the host | **Blocked** | Workloads run as an unprivileged uid with `socket(AF_VSOCK, …)` refused. Checked at every start (§4.2, §4.4). | M1 |
| Lie about the machine's identity to the host | **Blocked** | The connection itself names the machine. Nothing the guest says about its identity is trusted (§4.4). | M1 |
| Run the host out of CPU, memory or disk | **Kept inside** | cgroup limits on the VMM, and a disk size per machine (§4.2). | M1 |
| Fork-bomb inside the guest | **Kept inside** | The `pids` limit is enforced in the guest by the guest agent (§4.2). | M2 |
| Keep a machine alive forever | **Blocked** | Idle and max TTL, and an exec timeout (§5). | M1 |
| Flood the host with output or events | **Blocked** | Output is capped per exec; frames are capped at 1 MiB (§4.4). | M1 |

## 2. Reaching other places on the network

| Attack | Result | How | From |
|---|---|---|---|
| Connect anywhere from an `egress: none` machine | **Blocked** | The machine gets no network device. The host firewall is the second layer. TSI is off. A wall check at every start fails the machine if any of this is wrong (§4.2, §7.3). | M1 |
| Reach the LAN, the host or another machine | **Blocked** | The only route out is the egress proxy. The proxy connects only to globally reachable addresses (§7.3). | M2 |
| Reach cloud metadata (`169.254.169.254` and similar) | **Blocked** | Not a globally reachable address (§7.3). | M2 |
| DNS rebinding: a name that resolves to a private address | **Blocked** | The proxy resolves names itself and checks the address it connects to (§7.3). | M2 |
| Hide a private address inside an IPv6 form (NAT64, IPv4-mapped, 6to4, Teredo) | **Blocked** | The global-address rule covers these forms. The proxy connects upstream on IPv4 only (§7.3). | M2 |
| Reach the LAN through the site's own public IP (hairpin NAT on a home router) | **Blocked**, if the operator lists the address | The operator deny list holds the host's and site's own public addresses (§7.3). | M2 |
| Send data to a host that is not on the allowlist | **Blocked** | The proxy refuses it and records it (§7.3). | M2 |
| Domain fronting: an allowed name in SNI, another site behind the same CDN | **Narrowed, not closed** | Intercepted rules check that the HTTP authority equals SNI and the rule's host. Opaque rules cannot; their claim is only "connections to this SNI" (§7.3). | M2 |
| Send data out through an allowed host that stores it (a gist, a package publish, a model API upload) | **Narrowed, not closed** | Presets allow read methods and paths where the service allows it. Any allowed host that accepts data is still a way out (§7.3). | M2 |
| Ask an allowed API to fetch a URL for you (model web fetch or search tools) | **Left to you** | The platform cannot turn these tools off per request. Use a key with them turned off where the provider offers one (§7.3). | — |
| Talk over UDP, ICMP or IPv6 from the guest | **Blocked** | Dropped. DNS is answered only by the proxy (§7.3). | M2 |

## 3. Stealing or misusing credentials

| Attack | Result | How | From |
|---|---|---|---|
| Read the real API key from the environment, disk or memory | **Blocked** | The guest holds only a placeholder. The real value lives in the vault and in proxy memory (§8). | M3 |
| Send the placeholder to a host the attacker controls | **Blocked** | The proxy swaps the placeholder only for the grant's host, method and path, in the one configured header. Elsewhere the request is rejected and logged as a probe (§8). | M3 |
| Use the granted key for calls outside the grant | **Blocked** | Method and path are checked on intercepted traffic (§8). | M3 |
| Make the upstream echo the key back (an endpoint that returns request headers) | **Narrowed, not closed** | Grants name narrow, reviewed endpoints. Once the key reaches the upstream, the upstream decides (§8, trust limit). | M3 |
| Follow a redirect to carry the key to another host | **Blocked** | The proxy does not follow redirects on credentialed requests (§8). | M3 |
| Mint a new credential that works without the proxy (add an SSH key, a deploy key, a webhook; mint a short-lived token) | **Narrowed, not closed** | The operator must state the credential cannot mint credentials. A deny list of known minting endpoints is the second layer. Single-endpoint APIs (GraphQL, batch) cannot be granted (§8.9). | M3 |
| Steal the proxy's CA key to forge certificates for other machines | **Narrowed, not closed** | One CA per machine, name-constrained to that machine's hosts, held only in proxy memory. A compromised proxy still reaches every machine on its host (§8). | M2 |
| Keep using a key after it is revoked | **Narrowed, not closed** | Revocations are pushed at once; cached secrets expire within 60 seconds (§8). | M3 |
| Pin certificates to detect the proxy and refuse to work | **Kept inside** | Pinned clients fail closed. Nothing leaks (§8). | M3 |

## 4. Hiding what was done

| Attack | Result | How | From |
|---|---|---|---|
| Edit, remove or reorder audit records | **Blocked** up to the latest seal | Records are hash-chained and sealed; seals are copied off the host (§9). | M3 |
| Delete the tail of the log after the last seal | **Narrowed, not closed** | Records after the latest seal a verifier holds are not yet protected. A destroyed machine's log is complete only if it ends in a final seal (§9). | M3 |
| Do things inside the guest that cross no platform interface | **Left to you** | The audit log records API actions, egress decisions and policy violations, not what happens inside the guest (§2, §9). Use separate machines for separate trust levels. | — |

## 5. Attacking the operator through results

| Attack | Result | How | From |
|---|---|---|---|
| Plant a hook, filter or config in a repo that runs on the host when work is fetched | **Blocked** | Work leaves as a git bundle. The host fetches it in a jailed process into a fresh scratch repo, with `fsckObjects` on and one explicit refspec (§11.3). | M2 |
| Send a hostile pack to exploit the host's git parser or exhaust it | **Kept inside** the jail | The fetch process has no network, no host secrets and CPU, memory, time and size caps (§11.3). | M2 |
| Plant symlinks, submodules or `.gitattributes` filters that act on checkout | **Kept inside** | Fetched work is checked out or merged inside a machine, never in an operator checkout (§11.3). | M2 |
| Print terminal control codes in output or in a blocked host name | **Blocked** | Guest text is cut and escaped before it is printed (§7.3; M0 `sanitize`). | M0 |
| Ship malicious code in a template that a build produced | **Left to you** | A template signature means "this control plane approved these bytes", not "this software is safe". Builds run in untrusted machines (§12). | — |
| Swap a template on the registry or host disk | **Blocked** | The host agent checks the template's signature and content hash before boot (§12). | M3 |
| Swap a host binary in a release | **Blocked** | Binaries are verified against a release key pinned at install (§12). | M2 |

## 6. Machines attacking each other

| Attack | Result | How | From |
|---|---|---|---|
| One machine reaches another over the network | **Blocked** | No route between machines (§7.3). | M1 |
| A process in one exec spies on another exec in the same machine | **Narrowed, not closed** | Per-exec confinement is defence in depth, not a trust boundary. Work at different trust levels runs in separate machines (§7.2). | M2 |
| Untrusted code reaches a production service | **Blocked** | `untrusted` and `services` pools are on separate hosts (§3, §12). | M4 |

## 7. Host failures

| Failure | Result | How | From |
|---|---|---|---|
| Another program flushes or changes sandpiper's firewall rules | **Blocked** | A timer checks the nftables table hash. On a change, the host stops new starts and takes down running machines' links (§4.2). | M1 |
| A VMM is started with TSI on, or with a network device on an `egress: none` machine | **Blocked** | The wall check marks the machine `failed`, using a canary inside the VMM's own namespace (§4.2). | M1 |
| The proxy restarts and loses its CA keys | **Blocked** (fails closed) | Intercepted traffic from existing machines fails. Ephemeral machines are replaced; services are redeployed (§8). | M2 |

---

## Keeping this page true

- A change to `DESIGN.md` that adds, removes or weakens a wall changes this
  page in the same pull request.
- A row marked **Blocked** needs a test in the escape suite by its milestone.
  A row with no test says so.
- A new attack goes in the table that fits, with its result, even if the
  result is **Left to you**.
