# Host setup

How to turn a spare machine into a sandpiper host that an agent can reach over
SSH. Repeat per machine; number them `sandpiper-1`, `sandpiper-2`, …

## 1. Check the hardware

- x86_64 CPU with Intel VT-x or AMD-V (nearly every Intel/AMD CPU since ~2010).
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
grep -Ec '(vmx|svm)' /proc/cpuinfo      # must print a number above 0
ls -l /dev/kvm                          # must exist
sudo apt update && sudo apt install -y cpu-checker
kvm-ok                                  # must say "KVM acceleration can be used"
sudo usermod -aG kvm sandpiper
```

If `/dev/kvm` is missing, step 2 did not take.

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
