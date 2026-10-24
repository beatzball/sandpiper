# sandpiper

Self-hosted microVM machines for AI agents and apps.

One platform runs two kinds of workload on hardware you control:

- **sanderling** — short-lived machines for untrusted code. Boot in under a
  second, run behind a policy they cannot loosen, and are thrown away after.
- **plover** — long-lived services with a domain and TLS.

**Status:** M0 works on Apple Silicon: a microVM boots and runs a command
over vsock, about 120 ms warm. Development only — no isolation claims yet
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

## Layout

| Crate | What it is |
|---|---|
| `sandpiper-proto` | Frames between host and guest agent |
| `sandpiper-guest` | Guest agent: listens on vsock, runs commands |
| `sandpiper-vmm` | One microVM per process, through libkrun |
| `sandpiper` | Placeholder for the published client crate |

- [DESIGN.md](DESIGN.md) — architecture, policy model, API sketch, milestones
- [docs/host-setup.md](docs/host-setup.md) — turning a spare box into a KVM host

## License

[Apache-2.0](LICENSE)
