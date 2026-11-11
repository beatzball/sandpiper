# sandpiper

Self-hosted microVM machines for AI agents and apps.

One platform runs two kinds of workload on hardware you control:

- **sanderling** — short-lived machines for untrusted code. Boot in under a
  second, run behind a policy they cannot loosen, and are thrown away after.
- **plover** — long-lived services with a domain and TLS.

**Status:** M0 works on Apple Silicon: a microVM boots and runs a command
over vsock, about 120 ms warm. M1 has started on Linux KVM: machines boot
from their own disk image, the VMM runs sandboxed (own uid, namespaces,
seccomp, cgroup), and workloads run unprivileged and cannot reach vsock. The
host agent serves create, exec and destroy over HTTP, with a replayable
event log and time limits, checks every machine's walls before handing it
out, and keeps its own firewall table. Templates: Alpine, and Alpine with
Node.js. No isolation claims until M1 is done on a real
host
(see [DESIGN.md §12](DESIGN.md#12-hosting)).

## Try M0 (macOS, Apple Silicon)

```sh
brew tap slp/krun && brew trust slp/krun && brew install slp/krun/libkrun
rustup target add aarch64-unknown-linux-musl
scripts/m0.sh                      # echo hello
scripts/m0.sh uname -a             # any command in an Alpine guest
```

The script downloads and checks an Alpine minirootfs, cross-compiles the guest
agent, ad-hoc signs `sandpiper-vmm` with the hypervisor entitlement, and runs
the command. The guest has no network device.

## Try M1 so far (Linux with KVM)

```sh
scripts/install-libkrun-linux.sh          # once; see docs/host-setup.md §4.1
rustup target add "$(uname -m)-unknown-linux-musl"
scripts/m1-smoke.sh                       # builds the image, runs the checks
scripts/m1-api-smoke.sh                   # the host agent and its API
scripts/m1-wall-smoke.sh                  # wall checks, firewall, templates
```

To run the host agent as a service, see docs/host-setup.md §4.2.

On an M3 or newer Mac, a Lima VM with nested virtualization works for
development (docs/host-setup.md §0.1).

## Layout

| Crate | What it is |
|---|---|
| `sandpiper-proto` | Frames between host and guest agent |
| `sandpiper-guest` | Guest agent: listens on vsock, runs commands |
| `sandpiper-vmm` | One microVM per process, through libkrun |
| `sandpiperd` | Host agent: runs machines, serves the API, keeps the event log |
| `sandpiper` | Placeholder for the published client crate |

- [DESIGN.md](DESIGN.md) — architecture, policy model, API sketch, milestones
- [docs/host-setup.md](docs/host-setup.md) — turning a spare box into a KVM host

## License

[Apache-2.0](LICENSE)
