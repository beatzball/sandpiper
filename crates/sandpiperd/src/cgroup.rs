//! One cgroup (v2) per machine (DESIGN.md §4.2): CPU, memory, process and
//! disk I/O limits from outside the guest, and `cgroup.kill` as the final
//! stop. Linux only.
//!
//! Layout. Under systemd with `Delegate=yes`, the host agent's own service
//! cgroup is the base: the agent moves itself into `<base>/agent`, because a
//! cgroup that hands controllers to children may hold no processes itself.
//! Run any other way (by hand, for development), the base is
//! `/sys/fs/cgroup/sandpiper`. Machines are `<base>/<machine id>`.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const MOUNT: &str = "/sys/fs/cgroup";
const CONTROLLERS: &str = "+cpu +memory +pids +io";

/// Memory the VMM process needs beyond the guest's RAM.
const VMM_OVERHEAD_MIB: u64 = 256;
/// Threads and processes of one machine: its supervisor, VMM and relay.
const MAX_PIDS: u32 = 1024;
/// Disk I/O ceiling per machine.
const IO_BPS: u64 = 200 << 20;
const IO_IOPS: u64 = 4000;

#[derive(Clone, Copy)]
pub struct Limits {
    pub cpus: u8,
    pub mem_mib: u32,
}

pub struct Root {
    base: PathBuf,
    /// The whole disk that holds machine images, as `major:minor`, if it
    /// has one.
    disk: Option<String>,
}

fn write(path: &Path, value: &str) -> io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|mut f| f.write_all(value.as_bytes()))
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("write {value:?} to {}: {e}", path.display()),
            )
        })
}

impl Root {
    /// Find or make the base cgroup and turn on its controllers. `state` is
    /// the directory machine images live in; its disk gets the I/O limit.
    pub fn setup(state: &Path) -> io::Result<Self> {
        let own = fs::read_to_string("/proc/self/cgroup")?;
        let own = own
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .ok_or_else(|| io::Error::other("cgroup v2 is required"))?
            .to_owned();
        let delegated = std::env::var_os("INVOCATION_ID").is_some() && own.ends_with(".service");
        let base = if delegated {
            let base = Path::new(MOUNT).join(own.trim_start_matches('/'));
            let agent = base.join("agent");
            fs::create_dir_all(&agent)?;
            write(&agent.join("cgroup.procs"), "0")?;
            base
        } else {
            let base = Path::new(MOUNT).join("sandpiper");
            fs::create_dir_all(&base)?;
            write(
                &Path::new(MOUNT).join("cgroup.subtree_control"),
                CONTROLLERS,
            )?;
            base
        };
        write(&base.join("cgroup.subtree_control"), CONTROLLERS)?;
        let disk = match whole_disk(state) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("sandpiperd: machines get no disk I/O limit: {e}");
                None
            }
        };
        Ok(Self { disk, base })
    }

    /// Kill and remove machine cgroups a previous run left behind.
    pub fn remove_leftovers(&self) {
        let Ok(dir) = fs::read_dir(&self.base) else {
            return;
        };
        for entry in dir.flatten() {
            let name = entry.file_name();
            if crate::ids::is_id(&name.to_string_lossy(), "m") {
                let g = Group { path: entry.path() };
                let _ = g.kill();
                let _ = g.remove();
            }
        }
    }

    pub fn create(&self, id: &str, limits: Limits) -> io::Result<Group> {
        let path = self.base.join(id);
        fs::create_dir(&path)?;
        let g = Group { path };
        let set = |file: &str, value: String| write(&g.path.join(file), &value);
        let applied = set(
            "memory.max",
            ((u64::from(limits.mem_mib) + VMM_OVERHEAD_MIB) << 20).to_string(),
        )
        .and_then(|()| match set("memory.swap.max", "0".into()) {
            // No swap accounting: there is no swap to limit.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        })
        .and_then(|()| set("pids.max", MAX_PIDS.to_string()))
        .and_then(|()| {
            set(
                "cpu.max",
                format!("{} 100000", u32::from(limits.cpus) * 100_000),
            )
        })
        .and_then(|()| match &self.disk {
            Some(disk) => set(
                "io.max",
                format!("{disk} rbps={IO_BPS} wbps={IO_BPS} riops={IO_IOPS} wiops={IO_IOPS}"),
            ),
            None => Ok(()),
        });
        if let Err(e) = applied {
            let _ = g.remove();
            return Err(e);
        }
        Ok(g)
    }
}

#[derive(Clone)]
pub struct Group {
    path: PathBuf,
}

impl Group {
    /// Open `cgroup.procs` for a child to write `0` into before it execs,
    /// so it starts inside the group.
    pub fn procs(&self) -> io::Result<fs::File> {
        fs::OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.procs"))
    }

    /// SIGKILL every process in the group at once.
    pub fn kill(&self) -> io::Result<()> {
        write(&self.path.join("cgroup.kill"), "1")
    }

    /// Whether any process is left in the group.
    pub fn populated(&self) -> bool {
        fs::read_to_string(self.path.join("cgroup.events"))
            .map(|s| s.lines().any(|l| l == "populated 1"))
            .unwrap_or(false)
    }

    /// Remove the group once its processes are gone.
    pub fn remove(&self) -> io::Result<()> {
        for _ in 0..250 {
            match fs::remove_dir(&self.path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                // Busy until the last killed process has been reaped.
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
        fs::remove_dir(&self.path)
    }
}

/// `major:minor` of the whole disk under `path`. `io.max` takes only whole
/// disks, not partitions.
fn whole_disk(path: &Path) -> io::Result<String> {
    let dev = fs::metadata(path)?.dev();
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    if major == 0 {
        return Err(io::Error::other(format!(
            "{} is not on a block device, so its disk I/O cannot be limited",
            path.display()
        )));
    }
    let sys = fs::canonicalize(format!("/sys/dev/block/{major}:{minor}"))?;
    let disk = if sys.join("partition").exists() {
        sys.parent()
            .ok_or_else(|| io::Error::other("partition without a disk"))?
            .to_path_buf()
    } else {
        sys
    };
    Ok(fs::read_to_string(disk.join("dev"))?.trim().to_owned())
}
