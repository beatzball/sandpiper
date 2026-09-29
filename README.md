# sandpiper

Self-hosted microVM machines for AI agents and apps.

One platform runs two kinds of workload on hardware you control:

- **sanderling** — short-lived machines for untrusted code. Boot in under a
  second, run behind a policy they cannot loosen, and are thrown away after.
- **plover** — long-lived services with a domain and TLS.

**Status:** design stage. Nothing runs yet.

- [DESIGN.md](DESIGN.md) — architecture, policy model, API sketch, milestones
- [docs/host-setup.md](docs/host-setup.md) — turning a spare box into a KVM host

## License

[Apache-2.0](LICENSE)
