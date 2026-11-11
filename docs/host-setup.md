# Host setup

How to turn a spare machine into a sandpiper host that an agent can reach over
SSH. Repeat per machine; number them `sandpiper-1`, `sandpiper-2`, …

## 0. macOS (M0 development only)

An Apple Silicon Mac runs M0 with libkrun on Hypervisor.framework. There is no
firmware or OS setup:

```sh
brew tap slp/krun && brew trust slp/krun
brew install slp/krun/libkrun
```

A Mac host is for development only and must never run untrusted code — see
DESIGN.md §12.

## 0.1 A Linux KVM dev VM on the Mac (M1 development only)

An M3 or newer Mac on macOS 15 or later can give a Linux VM its own `/dev/kvm`
(nested virtualization). That is enough to build and test M1 without a spare
machine. It is slower than real hardware and it is **not** an isolation
boundary: the whole stack runs inside your Mac. Use a real Linux host
(sections 1–7) before any isolation testing.

```sh
brew install lima
limactl create --name=sandpiper-dev --tty=false --vm-type=vz \
  --cpus=4 --memory=8 --disk=40 \
  --set '.nestedVirtualization=true | .mounts=[{"location":"'"$PWD"'","writable":false}]' \
  template:ubuntu-24.04
limactl start sandpiper-dev
```

Run this from the repository root. It mounts only the repository, read-only.
Build inside the VM with a target directory of its own.

Check KVM inside the VM:

```sh
limactl shell sandpiper-dev bash -lc 'uname -m && ls -l /dev/kvm'
```

Expect `aarch64` and a `/dev/kvm` character device. Then install the build
tools in the VM:

```sh
limactl shell sandpiper-dev bash -lc '
  sudo apt-get update && sudo apt-get install -y build-essential pkg-config git curl nftables e2fsprogs libclang-dev rsync
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
```

Then install libkrun (section 4.1) inside the VM:

```sh
limactl shell sandpiper-dev bash -lc '. ~/.cargo/env && scripts/install-libkrun-linux.sh'
```

Stop it with `limactl stop sandpiper-dev` when you are not using it.

The rest of this guide is for real Linux hosts.

## 1. Check the hardware

- x86_64 CPU with Intel VT-x or AMD-V (nearly every Intel/AMD CPU since ~2010),
  or an arm64 board whose kernel exposes `/dev/kvm`.
- 16 GB RAM or more is comfortable; 8 GB works for M0.
- An SSD.
- Wired network if possible.

## 2. Turn on virtualization in the firmware

Reboot into BIOS/UEFI setup (usually `Del`, `F2` or `F10`). Enable:

- **Intel:** "Intel Virtualization Technology" (VT-x). Also VT-d if present.
- **AMD:** "SVM Mode". Also IOMMU if present.

Save and exit.

## 3. Install the OS

Install **Ubuntu Server 24.04 LTS**.

- Hostname: `sandpiper-1`
- User: `sandpiper`
- Tick **Install OpenSSH server**.
- Skip the optional snaps.

After install, give the box a fixed address: a DHCP reservation in your router
is simplest.

## 4. Check KVM on the box

Log in on the box itself and run:

```sh
ls -l /dev/kvm                          # must exist (any architecture)

# x86_64 only:
grep -Ec '(vmx|svm)' /proc/cpuinfo      # must print a number above 0
sudo apt update && sudo apt install -y cpu-checker
kvm-ok                                  # must say "KVM acceleration can be used"
```

If `/dev/kvm` is missing, step 2 did not take.

Do not add `sandpiper` to the `kvm` group. It is the operator account and gets
sudo in step 6. MicroVMs run under separate unprivileged users that the host
agent creates (DESIGN.md §4.2), so a VM escape never lands in an account with
sudo.

### 4.1 Install libkrun

From a checkout of this repository, on the box:

```sh
sudo apt-get install -y build-essential pkg-config git curl e2fsprogs libclang-dev
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
scripts/install-libkrun-linux.sh
```

The script installs a pinned, checksum-verified libkrunfw (the guest kernel)
and builds the pinned libkrun with block and network devices into
`/usr/local`. Then check the whole M1 stack:

```sh
rustup target add "$(uname -m)-unknown-linux-musl"
scripts/m1-smoke.sh
scripts/m1-api-smoke.sh
scripts/m1-wall-smoke.sh
```

`m1-wall-smoke.sh` also builds test binaries in `target/wall-tests` that
start machines with a wall open on purpose. Never install those.

### 4.2 Run the host agent

```sh
cargo build --release -p sandpiperd -p sandpiper-vmm
TEMPLATE=node scripts/m1-image.sh                 # Alpine + Node.js
sudo install -m 0755 target/release/sandpiperd target/release/sandpiper-vmm /usr/local/bin/
sudo install -D -m 0644 target/m1/base.img /var/lib/sandpiper/templates/base.img
sudo install -D -m 0644 target/m1/node.img /var/lib/sandpiper/templates/node.img
sudo install -m 0644 packaging/sandpiperd.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now sandpiperd
```

The host agent refuses to start if SMT (hyper-threading) is on, or if any
file in `/sys/devices/system/cpu/vulnerabilities/` starts with
`Vulnerable` (DESIGN.md §12). Turn SMT off with `nosmt` on the kernel
command line (in `/etc/default/grub`, then `sudo update-grub` and reboot),
and keep mitigations on. Update the kernel and CPU microcode for the rest.
Only on a box that never runs untrusted code, such as the dev VM, pass
`--allow-weak-host` instead.

At start it installs its own nftables table, `inet sandpiper`, and checks
it every 10 s. If another program changes it, `GET /v1/health` reports why
and no new machine starts until the host agent is restarted. Every machine
is wall-checked before it is handed out; a machine that fails is
`failed`, with the reason in its events.

The API listens on `127.0.0.1:7171`. Its token is in
`/var/lib/sandpiper/token` (root only). Try it:

```sh
T=$(sudo cat /var/lib/sandpiper/token)
curl -s -H "Authorization: Bearer $T" -d '{"template":"base"}' \
  -H 'Content-Type: application/json' http://127.0.0.1:7171/v1/machines
curl -s -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  -d '{"cmd":["uname","-a"]}' http://127.0.0.1:7171/v1/machines/<id>/exec
```

The exec reply is a stream of events, one JSON object per line; output is
base64 in `data`. Each stream of one exec keeps its first 1 MiB. A machine
may put 64 MiB of output into the event log in all; past that it is
stopped.

## 5. Give the agent SSH access (run on your Mac)

Use a key for sandpiper hosts only. The `-C` comment is public inside the key,
so it holds no name or email.

```sh
ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519_sandpiper -C sandpiper-agent
ssh-copy-id -i ~/.ssh/id_ed25519_sandpiper.pub sandpiper@<box-ip>
```

Add to `~/.ssh/config`:

```
Host sandpiper-1
  HostName <box-ip>
  User sandpiper
  IdentityFile ~/.ssh/id_ed25519_sandpiper
  IdentitiesOnly yes
```

Test:

```sh
ssh sandpiper-1 'uname -r && ls -l /dev/kvm'
```

## 6. Lock the box down (run on the box)

```sh
# Keys only, no passwords, over SSH.
echo 'PasswordAuthentication no' | sudo tee /etc/ssh/sshd_config.d/10-sandpiper.conf
sudo systemctl restart ssh

# Firewall: SSH in, nothing else yet.
sudo ufw allow OpenSSH && sudo ufw --force enable

# Let the agent install packages without a password prompt.
# Only do this on a box used for nothing else.
echo 'sandpiper ALL=(ALL) NOPASSWD:ALL' | sudo tee /etc/sudoers.d/sandpiper
sudo chmod 440 /etc/sudoers.d/sandpiper
```

Test the key login again from the Mac **before** closing the box's own
session, so a mistake cannot lock you out.

## 7. Not on the same network as the Mac?

Install Tailscale on both and use the box's Tailscale name as `HostName`.
Do not open SSH to the internet.
