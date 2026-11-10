//! Machines on this host (DESIGN.md §4.2, §5): create, exec and destroy,
//! with time limits. Everything here blocks; the API calls it from blocking
//! tasks.
//!
//! Each machine is one `sandpiper-vmm boot` process. On a Linux host it is
//! sandboxed: its own uid from [`UID_BASE`], a jail, its own cgroup. The
//! host agent never reads the guest's raw frames: it talks to the relay in
//! the VMM process (see `sandpiper-vmm`'s `relay`) over a socket the host
//! agent made, in a directory only root can reach.
//!
//! What a machine sends is still untrusted here: a VMM taken over could
//! send anything the frame format allows. So the host keeps its own limits
//! on each exec's output and on all a machine may write to the event log.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sandpiper_proto::{
    DEFAULT_OUTPUT_CAP, ExecRequest, Frame, PROTOCOL_VERSION, read_frame, write_frame,
};
use serde_json::json;

use crate::events::{Log, now_ms};
use crate::ids;

/// Per-machine uids and gids are `UID_BASE + slot`: far above login users,
/// the usual /etc/subuid ranges, and the ids `sandpiper-vmm run` uses.
pub const UID_BASE: u32 = 1_900_000_000;

const BOOT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an exec waits for the relay's Ready. Longer than boot: a busy
/// guest answers only after its previous exchange is done.
const EXEC_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a machine gets to stop after its lifeline closes, before
/// `cgroup.kill`.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long past an exec's timeout the host waits for the guest to stop it.
/// The guest is the real enforcer; this only catches one that never answers.
const HOST_TIMEOUT_GRACE: Duration = Duration::from_secs(15);
/// How long a destroy waits for an exec in flight to record its end.
const EXEC_SETTLE: Duration = Duration::from_secs(5);
const CONSOLE_TAIL: usize = 16 * 1024;
const STDIN_CHUNK: usize = 32 * 1024;
/// Output is written to the log in pieces of at most this size, or after
/// [`FLUSH_EVERY`], whichever comes first.
const OUTPUT_PIECE: usize = 64 * 1024;
const FLUSH_EVERY: Duration = Duration::from_millis(100);
/// Most `Error` frames one exec may send.
const MAX_ERRORS: usize = 16;
/// Most bytes of guest data one machine may put into the event log.
const MACHINE_LOG_QUOTA: u64 = 64 << 20;
/// Most bytes of command, environment and working directory in one exec.
pub const MAX_REQUEST: usize = 256 << 10;

#[derive(Clone)]
pub struct Config {
    pub state: PathBuf,
    pub vmm: PathBuf,
    /// Sandbox each VMM (Linux, as root). Off only for development.
    pub sandbox: bool,
    pub max_machines: usize,
    pub cpus: u8,
    pub mem_mib: u32,
    pub idle: Duration,
    pub max: Duration,
    pub exec_max: Duration,
}

/// An error with the HTTP status it maps to.
#[derive(Debug)]
pub struct Error {
    pub status: u16,
    pub message: String,
}

impl Error {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::new(500, e.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Starting,
    Running,
    Stopping,
}

struct Machine {
    id: String,
    template: String,
    slot: u32,
    uid: Option<u32>,
    dir: PathBuf,
    created: Instant,
    created_ms: u64,
    idle: Duration,
    max: Duration,
    child: Mutex<Child>,
    /// Closing it tells the VMM's supervisor to kill the machine.
    lifeline: Mutex<Option<io::PipeWriter>>,
    cgroup: Option<CgroupGroup>,
    state: Mutex<State>,
    /// Why the machine is stopping, once it is.
    reason: Mutex<Option<String>>,
    last_active: Mutex<Instant>,
    busy: AtomicBool,
    /// The exec whose end is not yet recorded. Whoever takes it records
    /// the end, exactly once: the exec itself, or a destroy.
    active: Mutex<Option<String>>,
    execs: AtomicU64,
    /// Bytes of guest data this machine has put into the event log.
    logged: AtomicU64,
    console: Arc<Mutex<VecDeque<u8>>>,
}

impl Machine {
    fn relay(&self) -> PathBuf {
        self.dir.join("host.sock")
    }

    fn state(&self) -> State {
        *self.state.lock().expect("state lock")
    }

    fn info(&self) -> serde_json::Value {
        let state = match self.state() {
            State::Starting => "starting",
            State::Running => "running",
            State::Stopping => "stopping",
        };
        json!({
            "id": self.id,
            "state": state,
            "reason": *self.reason.lock().expect("reason lock"),
            "template": self.template,
            "createdAt": self.created_ms,
            "expiresAt": self.created_ms + self.max.as_millis() as u64,
            "idleMs": self.idle.as_millis() as u64,
        })
    }

    fn console_tail(&self) -> String {
        let t = self.console.lock().expect("console lock");
        let bytes: Vec<u8> = t.iter().copied().collect();
        crate::sanitize(&String::from_utf8_lossy(&bytes), CONSOLE_TAIL)
    }

    /// Count `n` bytes against the machine's log quota; false once over.
    fn charge(&self, n: usize) -> bool {
        self.logged.fetch_add(n as u64, Ordering::SeqCst) + n as u64 <= MACHINE_LOG_QUOTA
    }
}

/// Spawns VMM processes from one thread that never exits. A sandboxed
/// VMM's supervisor dies with the *thread* that spawned it
/// (`PR_SET_PDEATHSIG`), so short-lived worker threads cannot do it.
type SpawnJob = (Command, mpsc::Sender<io::Result<Child>>);

struct Spawner(Mutex<mpsc::Sender<SpawnJob>>);

impl Spawner {
    fn start() -> Self {
        let (tx, rx) = mpsc::channel::<SpawnJob>();
        thread::Builder::new()
            .name("vmm-spawner".into())
            .spawn(move || {
                for (mut cmd, reply) in rx {
                    let _ = reply.send(cmd.spawn());
                }
            })
            .expect("spawn thread");
        Self(Mutex::new(tx))
    }

    fn spawn(&self, cmd: Command) -> io::Result<Child> {
        let (tx, rx) = mpsc::channel();
        self.0
            .lock()
            .expect("spawner lock")
            .send((cmd, tx))
            .map_err(|_| io::Error::other("spawner thread is gone"))?;
        rx.recv()
            .map_err(|_| io::Error::other("spawner thread is gone"))?
    }
}

pub struct Manager {
    cfg: Config,
    log: Arc<Log>,
    machines: Mutex<HashMap<String, Arc<Machine>>>,
    /// Slots in use, by live machines and by quarantined uids.
    slots: Mutex<BTreeSet<u32>>,
    /// Machines whose cgroup could not be emptied: their uid is not handed
    /// out again until it is.
    /// Each with the slots it holds, which may be none.
    quarantine: Mutex<Vec<(Vec<u32>, CgroupGroup)>>,
    spawner: Spawner,
    /// Set at shutdown: no new machines or execs.
    stopping: AtomicBool,
    /// Creates in progress, which shutdown waits for.
    creating: AtomicUsize,
    #[cfg(target_os = "linux")]
    cgroups: Option<crate::cgroup::Root>,
}

pub struct CreateRequest {
    pub template: String,
    pub idle: Option<Duration>,
    pub max: Option<Duration>,
    /// The caller's own data, kept with the machine's first event.
    pub metadata: Option<serde_json::Value>,
}

pub struct ExecArgs {
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub stdin: Vec<u8>,
    pub timeout: Option<Duration>,
}

impl ExecArgs {
    /// Bytes of command, environment and working directory, as JSON.
    pub fn request_size(&self) -> usize {
        serde_json::to_vec(&(&self.cmd, &self.env, &self.cwd)).map_or(usize::MAX, |v| v.len())
    }
}

impl Manager {
    /// Set up the host: state directories, cgroups, and cleanup of anything
    /// a previous run left behind.
    pub fn start(cfg: Config, log: Arc<Log>) -> io::Result<Arc<Self>> {
        for dir in ["machines", "templates"] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(cfg.state.join(dir))?;
        }
        // A machine's uid is free only once its processes are gone. Uids of
        // leftovers that survive cgroup.kill stay taken until they are.
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut slots = BTreeSet::new();
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut quarantine = Vec::new();
        #[cfg(target_os = "linux")]
        let cgroups = if cfg.sandbox {
            let root = crate::cgroup::Root::setup(&cfg.state.join("machines"))?;
            for (uids, g) in root.remove_leftovers() {
                let mine: Vec<u32> = uids
                    .into_iter()
                    .filter_map(|u| u.checked_sub(UID_BASE))
                    .collect();
                eprintln!(
                    "sandpiperd: a leftover machine cgroup keeps processes; slots {mine:?} wait for it"
                );
                slots.extend(&mine);
                // Kept even with no slot, so the reaper goes on removing it.
                quarantine.push((mine, g));
            }
            Some(root)
        } else {
            None
        };
        // Machines die with the host agent; their files and records stay.
        for entry in fs::read_dir(cfg.state.join("machines"))?.flatten() {
            let _ = fs::remove_dir_all(entry.path());
        }
        for id in log.unfinished()? {
            log.append(
                &id,
                "machine.failed",
                json!({"reason": "host agent restarted"}),
            )?;
            log.append(
                &id,
                "machine.destroyed",
                json!({"reason": "host agent restarted"}),
            )?;
        }
        let m = Arc::new(Self {
            cfg,
            log,
            machines: Mutex::new(HashMap::new()),
            slots: Mutex::new(slots),
            quarantine: Mutex::new(quarantine),
            spawner: Spawner::start(),
            stopping: AtomicBool::new(false),
            creating: AtomicUsize::new(0),
            #[cfg(target_os = "linux")]
            cgroups,
        });
        let reaper = Arc::downgrade(&m);
        thread::Builder::new()
            .name("reaper".into())
            .spawn(move || {
                while let Some(m) = reaper.upgrade() {
                    m.reap();
                    drop(m);
                    thread::sleep(Duration::from_millis(250));
                }
            })
            .expect("spawn thread");
        Ok(m)
    }

    pub fn create(&self, req: CreateRequest) -> Result<serde_json::Value> {
        // Counted before the check, so a shutdown that sets `stopping` either
        // turns this away or waits for it.
        self.creating.fetch_add(1, Ordering::SeqCst);
        let _done = Decrement(&self.creating);
        if self.stopping.load(Ordering::SeqCst) {
            return Err(Error::new(503, "the host agent is stopping"));
        }
        let image = self.template_image(&req.template)?;
        let idle = req.idle.unwrap_or(self.cfg.idle);
        let max = req.max.unwrap_or(self.cfg.max);
        if idle > self.cfg.idle || max > self.cfg.max || idle.is_zero() || max.is_zero() {
            return Err(Error::new(
                400,
                format!(
                    "ttl must be positive and at most idleMs {} and maxMs {}",
                    self.cfg.idle.as_millis(),
                    self.cfg.max.as_millis()
                ),
            ));
        }
        let slot = self.alloc_slot()?;
        let uid = self.cfg.sandbox.then_some(UID_BASE + slot);
        let id = ids::new_id("m");
        if let Err(e) = self.log.append(
            &id,
            "machine.created",
            json!({"template": req.template, "metadata": req.metadata}),
        ) {
            self.free_slot(slot);
            return Err(e.into());
        }
        let started = Instant::now();
        let dir = self.cfg.state.join("machines").join(&id);
        let (child, lifeline, console, cgroup) = match self.launch(&id, &dir, &image, uid) {
            Ok(parts) => parts,
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                self.free_slot(slot);
                let _ = self.finish(&id, Some(&e.to_string()), "failed to start");
                return Err(Error::new(500, format!("machine failed to start: {e}")));
            }
        };
        let machine = Arc::new(Machine {
            id: id.clone(),
            template: req.template,
            slot,
            uid,
            dir,
            created: started,
            created_ms: now_ms(),
            idle,
            max,
            child: Mutex::new(child),
            lifeline: Mutex::new(Some(lifeline)),
            cgroup,
            state: Mutex::new(State::Starting),
            reason: Mutex::new(None),
            last_active: Mutex::new(Instant::now()),
            busy: AtomicBool::new(false),
            active: Mutex::new(None),
            execs: AtomicU64::new(0),
            logged: AtomicU64::new(0),
            console,
        });
        self.machines
            .lock()
            .expect("machines lock")
            .insert(id.clone(), Arc::clone(&machine));
        // A shutdown that took its list of machines before this one was in
        // it set `stopping` first, so this sees it and cleans up itself.
        if self.stopping.load(Ordering::SeqCst) {
            self.destroy_machine(&machine, "host agent stopping", false);
            return Err(Error::new(503, "the host agent is stopping"));
        }
        if let Err(e) = self.wait_ready(&machine) {
            let reason = format!("machine failed to start: {e}");
            self.destroy_machine(&machine, &reason, true);
            return Err(Error::new(500, reason));
        }
        // Under the state lock, so a destroy cannot slip its events between
        // the check and the record.
        let mut state = machine.state.lock().expect("state lock");
        if *state != State::Starting {
            return Err(Error::new(409, "machine was destroyed while it started"));
        }
        let ready_ms = started.elapsed().as_millis() as u64;
        if let Err(e) = self
            .log
            .append(&id, "machine.running", json!({"readyMs": ready_ms}))
        {
            drop(state);
            self.destroy_machine(&machine, &format!("event log failed: {e}"), true);
            return Err(e.into());
        }
        *state = State::Running;
        drop(state);
        *machine.last_active.lock().expect("lock") = Instant::now();
        Ok(machine.info())
    }

    fn template_image(&self, name: &str) -> Result<PathBuf> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.'))
            && !name.starts_with('.');
        let path = self.cfg.state.join("templates").join(format!("{name}.img"));
        if !ok || !path.is_file() {
            return Err(Error::new(404, format!("no template named {name:?}")));
        }
        Ok(path)
    }

    /// Reserve a machine slot, which also fixes its uid.
    fn alloc_slot(&self) -> Result<u32> {
        let live = self.machines.lock().expect("machines lock").len();
        let mut slots = self.slots.lock().expect("slots lock");
        if live >= self.cfg.max_machines || slots.len() >= self.cfg.max_machines * 2 {
            return Err(Error::new(503, "this host is full"));
        }
        let slot = (0..u32::MAX)
            .find(|s| !slots.contains(s))
            .expect("fewer machines than slots");
        slots.insert(slot);
        Ok(slot)
    }

    fn free_slot(&self, slot: u32) {
        self.slots.lock().expect("slots lock").remove(&slot);
    }

    #[allow(clippy::type_complexity)]
    fn launch(
        &self,
        id: &str,
        dir: &Path,
        image: &Path,
        uid: Option<u32>,
    ) -> io::Result<(
        Child,
        io::PipeWriter,
        Arc<Mutex<VecDeque<u8>>>,
        Option<CgroupGroup>,
    )> {
        // Only root can reach the machine's directory. The VMM sees just
        // its disk image, which is the one file its uid owns.
        fs::DirBuilder::new().mode(0o700).create(dir)?;
        let disk = dir.join("root.img");
        copy_image(image, &disk)?;
        let listener = UnixListener::bind(dir.join("host.sock"))?;

        let mut cmd = Command::new(&self.cfg.vmm);
        cmd.arg("boot")
            .arg("--disk")
            .arg(&disk)
            .args(["--cpus", &self.cfg.cpus.to_string()])
            .args(["--mem", &self.cfg.mem_mib.to_string()])
            .arg("--die-with-parent");
        let cgroup = match uid {
            Some(uid) => {
                std::os::unix::fs::chown(&disk, Some(uid), Some(uid))?;
                cmd.arg("--jail").arg(dir).args([
                    "--uid",
                    &uid.to_string(),
                    "--gid",
                    &uid.to_string(),
                ]);
                self.make_cgroup(id, &mut cmd)?
            }
            None => {
                cmd.arg("--socket")
                    .arg(dir.join("agent.sock"))
                    .env("SANDPIPER_UNSANDBOXED", "1");
                None
            }
        };
        pass_fd(&mut cmd, listener.as_raw_fd());
        cmd.args(["--relay-fd", &listener.as_raw_fd().to_string()]);
        let (lifeline_r, lifeline_w) = io::pipe()?;
        cmd.stdin(lifeline_r)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match self.spawner.spawn(cmd) {
            Ok(c) => c,
            Err(e) => {
                if let Some(g) = &cgroup {
                    remove_group(g);
                }
                return Err(e);
            }
        };
        drop(listener); // the child holds it now
        let console = Arc::new(Mutex::new(VecDeque::new()));
        capture(child.stdout.take().expect("piped"), &console);
        capture(child.stderr.take().expect("piped"), &console);
        Ok((child, lifeline_w, console, cgroup))
    }

    #[cfg(target_os = "linux")]
    fn make_cgroup(&self, id: &str, cmd: &mut Command) -> io::Result<Option<CgroupGroup>> {
        use std::os::unix::process::CommandExt;
        let Some(root) = &self.cgroups else {
            return Ok(None);
        };
        let group = root.create(
            id,
            crate::cgroup::Limits {
                cpus: self.cfg.cpus,
                mem_mib: self.cfg.mem_mib,
            },
        )?;
        let procs = group.procs()?;
        // SAFETY: the closure makes one write(2) call, which is safe
        // between fork and exec. `procs` stays open in the parent until
        // after spawn, and it is close-on-exec.
        unsafe {
            cmd.pre_exec(move || {
                let fd = procs.as_raw_fd();
                if libc::write(fd, b"0".as_ptr().cast(), 1) == 1 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        Ok(Some(group))
    }

    #[cfg(not(target_os = "linux"))]
    fn make_cgroup(&self, _: &str, _: &mut Command) -> io::Result<Option<CgroupGroup>> {
        Ok(None)
    }

    /// Wait until the relay answers Ready, which it sends once the guest
    /// agent has.
    fn wait_ready(&self, m: &Machine) -> io::Result<()> {
        let mut conn = UnixStream::connect(m.relay())?;
        conn.set_read_timeout(Some(BOOT_TIMEOUT))?;
        match expect_ready(&mut conn) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                Err(io::Error::other("timed out waiting for the guest agent"))
            }
            r => r,
        }
    }

    /// A machine in the map, in any state.
    fn lookup(&self, id: &str) -> Option<Arc<Machine>> {
        self.machines
            .lock()
            .expect("machines lock")
            .get(id)
            .cloned()
    }

    /// A machine's current info, or its last known state from the log.
    pub fn info(&self, id: &str) -> Result<serde_json::Value> {
        if let Some(m) = self.lookup(id) {
            return Ok(m.info());
        }
        let Some(last) = self.log.last_of(id, "machine.")? else {
            return Err(Error::new(404, format!("no machine {id}")));
        };
        let created = self.log.read(id, 1, 1)?.pop();
        let failed = self.log.last_of(id, "machine.failed")?.is_some();
        let state = match last.kind.as_str() {
            "machine.destroyed" if failed => "failed",
            "machine.destroyed" => "destroyed",
            // Between the log's first event and the machine's entry.
            _ => "starting",
        };
        Ok(json!({
            "id": id,
            "state": state,
            "template": created.as_ref().and_then(|e| e.str("template")),
            "createdAt": created.map(|e| e.time),
            "reason": last.str("reason"),
        }))
    }

    /// Record the start of an exec and return its ID and the sequence
    /// number of its first event. The exec itself runs in [`Self::run_exec`].
    pub fn begin_exec(&self, id: &str, args: &ExecArgs) -> Result<(String, u64)> {
        if args.cmd.is_empty() {
            return Err(Error::new(400, "cmd must not be empty"));
        }
        if args.request_size() > MAX_REQUEST {
            return Err(Error::new(
                400,
                format!("cmd, env and cwd are over {MAX_REQUEST} bytes"),
            ));
        }
        if let Some(t) = args.timeout
            && (t.is_zero() || t > self.cfg.exec_max)
        {
            return Err(Error::new(
                400,
                format!(
                    "timeoutMs must be positive and at most {}",
                    self.cfg.exec_max.as_millis()
                ),
            ));
        }
        if self.stopping.load(Ordering::SeqCst) {
            return Err(Error::new(503, "the host agent is stopping"));
        }
        let not_running = || Error::new(404, format!("no running machine {id}"));
        let m = self.lookup(id).ok_or_else(not_running)?;
        // Under the state lock: a destroy either sees this exec as busy and
        // waits for its end, or this sees the machine stopping.
        let state = m.state.lock().expect("state lock");
        match *state {
            State::Starting => return Err(Error::new(409, "the machine is still starting")),
            State::Stopping => return Err(not_running()),
            State::Running => {}
        }
        if m.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::new(
                409,
                "an exec is already running on this machine",
            ));
        }
        let exec = format!("e{}", m.execs.fetch_add(1, Ordering::SeqCst) + 1);
        *m.active.lock().expect("active lock") = Some(exec.clone());
        drop(state);
        *m.last_active.lock().expect("lock") = Instant::now();
        match self
            .log
            .append(id, "exec.started", json!({"exec": exec, "cmd": args.cmd}))
        {
            Ok(e) => Ok((exec, e.seq)),
            Err(e) => {
                *m.active.lock().expect("active lock") = None;
                m.busy.store(false, Ordering::SeqCst);
                Err(e.into())
            }
        }
    }

    /// Run an exec that [`Self::begin_exec`] recorded. Its output and
    /// result go to the event log; this returns when it has ended. Always
    /// records an end (`exec.exited`, `exec.timedout` or `exec.failed`).
    pub fn run_exec(&self, id: &str, exec: &str, args: ExecArgs) {
        // Gone already: its destroy recorded this exec's end.
        let Some(m) = self.lookup(id) else {
            return;
        };
        let timeout = args.timeout.unwrap_or(self.cfg.exec_max);
        let timed_out = Arc::new(AtomicBool::new(false));
        let result = self.exchange(&m, exec, args, timeout, &timed_out);
        let stopping = m.state() == State::Stopping;
        let backstop = timed_out.load(Ordering::SeqCst);
        let timed_out_event = || {
            (
                "exec.timedout",
                json!({"exec": exec, "timeoutMs": timeout.as_millis() as u64}),
            )
        };
        let (kind, data) = match &result {
            Ok(Ended::Exited(data)) => ("exec.exited", data.clone()),
            Ok(Ended::OverQuota) => (
                "exec.failed",
                json!({"exec": exec, "reason": "machine wrote too much to the event log"}),
            ),
            Ok(Ended::TimedOut) => timed_out_event(),
            Err(_) if backstop => timed_out_event(),
            Err(_) if stopping => (
                "exec.failed",
                json!({"exec": exec, "reason": "machine stopped"}),
            ),
            Err(e) => (
                "exec.failed",
                json!({"exec": exec, "reason": e.to_string()}),
            ),
        };
        let doom = match &result {
            _ if stopping => None,
            Ok(Ended::OverQuota) => Some("event log quota reached".to_owned()),
            // The guest did not stop the command at its timeout: it is not
            // doing its job, so the machine goes.
            Err(_) if backstop => Some("an exec did not stop at its timeout".to_owned()),
            // The relay did not even answer: the machine is wedged, and no
            // later exec would get through either.
            Err(Failure::NoAnswer(e)) => Some(format!("machine stopped answering: {e}")),
            _ => None,
        };
        // A doomed machine stops taking execs before this one lets go.
        let claimed = doom.as_ref().is_some_and(|r| self.begin_stop(&m, r));
        self.end_exec(&m, exec, kind, data);
        *m.last_active.lock().expect("lock") = Instant::now();
        m.busy.store(false, Ordering::SeqCst);
        if let (true, Some(reason)) = (claimed, doom) {
            self.teardown(&m, &reason, true);
        }
    }

    /// Record an exec's end, unless a destroy has already recorded it.
    fn end_exec(&self, m: &Machine, exec: &str, kind: &str, data: serde_json::Value) {
        let mut active = m.active.lock().expect("active lock");
        if active.as_deref() == Some(exec) {
            let _ = self.log.append(&m.id, kind, data);
            *active = None;
        }
    }

    fn exchange(
        &self,
        m: &Machine,
        exec: &str,
        args: ExecArgs,
        timeout: Duration,
        timed_out: &Arc<AtomicBool>,
    ) -> std::result::Result<Ended, Failure> {
        let mut conn = UnixStream::connect(m.relay()).map_err(Failure::NoAnswer)?;
        conn.set_read_timeout(Some(EXEC_READY_TIMEOUT))
            .map_err(Failure::Other)?;
        expect_ready(&mut conn).map_err(Failure::NoAnswer)?;
        conn.set_read_timeout(None).map_err(Failure::Other)?;

        // The guest kills the command at `timeout`. As a backstop, hanging
        // up a little later ends the exchange whatever it is blocked on.
        let hangup = conn.try_clone().map_err(Failure::Other)?;
        let _hangup_on_return = ShutdownOnDrop(conn.try_clone().map_err(Failure::Other)?);
        let done = Arc::new(AtomicBool::new(false));
        let _done_on_return = SetOnDrop(Arc::clone(&done));
        {
            let (done, timed_out) = (Arc::clone(&done), Arc::clone(timed_out));
            let deadline = Instant::now() + timeout + HOST_TIMEOUT_GRACE;
            thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    if Instant::now() >= deadline {
                        timed_out.store(true, Ordering::SeqCst);
                        let _ = hangup.shutdown(Shutdown::Both);
                        return;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
        }

        write_frame(
            &mut conn,
            &Frame::Exec(ExecRequest {
                argv: args.cmd,
                env: args.env,
                cwd: args.cwd,
                timeout_ms: Some(timeout.as_millis() as u64),
            }),
        )
        .map_err(Failure::Other)?;
        // Fed from its own thread so output is read while input is sent. It
        // ends when the input is sent or the connection is shut down.
        let mut feeder = conn.try_clone().map_err(Failure::Other)?;
        let stdin = args.stdin;
        let feeding = thread::spawn(move || -> io::Result<()> {
            for chunk in stdin.chunks(STDIN_CHUNK) {
                write_frame(&mut feeder, &Frame::Stdin(chunk.to_vec()))?;
            }
            write_frame(&mut feeder, &Frame::Stdin(Vec::new()))
        });
        let result = self.pass_output(m, exec, conn);
        drop(_hangup_on_return);
        let _ = feeding.join();
        result
    }

    /// Read the relay's frames and log them, with the host's own limits:
    /// each stream is cut at [`DEFAULT_OUTPUT_CAP`], at most [`MAX_ERRORS`]
    /// errors, and the machine's log quota. Output is gathered into pieces
    /// so that many small frames do not make many events.
    fn pass_output(
        &self,
        m: &Machine,
        exec: &str,
        conn: UnixStream,
    ) -> std::result::Result<Ended, Failure> {
        let frames = read_frames(conn);
        let mut out = OutputBuffer::new("exec.stdout");
        let mut err = OutputBuffer::new("exec.stderr");
        let mut errors = 0;
        let log = |kind: &str, data: serde_json::Value, size: usize| -> io::Result<bool> {
            if !m.charge(size) {
                return Ok(false);
            }
            self.log.append(&m.id, kind, data).map(|_| true)
        };
        let flush = |b: &mut OutputBuffer| -> io::Result<bool> {
            match b.take() {
                Some(data) => log(
                    b.kind,
                    json!({"exec": exec, "data": B64.encode(&data)}),
                    data.len(),
                ),
                None => Ok(true),
            }
        };
        loop {
            let frame = match frames.recv_timeout(FLUSH_EVERY) {
                Ok(f) => f,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !(flush(&mut out).map_err(Failure::Other)?
                        && flush(&mut err).map_err(Failure::Other)?)
                    {
                        return Ok(Ended::OverQuota);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Failure::Other(io::Error::other(
                        "the reader stopped without a reason",
                    )));
                }
            };
            let frame = frame.map_err(Failure::Other)?;
            let (b, other) = match frame {
                Some(Frame::Stdout(b)) => (b, &mut out),
                Some(Frame::Stderr(b)) => (b, &mut err),
                Some(Frame::Error(msg)) => {
                    errors += 1;
                    if errors > MAX_ERRORS {
                        return Err(Failure::Other(io::Error::other(
                            "machine sent too many errors",
                        )));
                    }
                    let msg = crate::sanitize(&msg, 4096);
                    let n = msg.len();
                    if !log("exec.error", json!({"exec": exec, "message": msg}), n)
                        .map_err(Failure::Other)?
                    {
                        return Ok(Ended::OverQuota);
                    }
                    continue;
                }
                Some(Frame::Exit(s)) => {
                    if !(flush(&mut out).map_err(Failure::Other)?
                        && flush(&mut err).map_err(Failure::Other)?)
                    {
                        return Ok(Ended::OverQuota);
                    }
                    if s.timed_out {
                        return Ok(Ended::TimedOut);
                    }
                    return Ok(Ended::Exited(json!({
                        "exec": exec,
                        "code": s.code,
                        "stdoutTruncated": s.stdout_truncated || out.cut,
                        "stderrTruncated": s.stderr_truncated || err.cut,
                    })));
                }
                Some(other) => {
                    return Err(Failure::Other(io::Error::other(format!(
                        "unexpected {} frame",
                        other.kind()
                    ))));
                }
                None => {
                    return Err(Failure::Other(io::Error::other(
                        "the machine closed the connection",
                    )));
                }
            };
            other.push(&b);
            // Both streams, every frame: a steady trickle on one must not
            // hold back the other, nor itself.
            for b in [&mut out, &mut err] {
                if b.due() && !flush(b).map_err(Failure::Other)? {
                    return Ok(Ended::OverQuota);
                }
            }
        }
    }

    pub fn destroy(&self, id: &str) -> Result<()> {
        let m = self
            .lookup(id)
            .ok_or_else(|| Error::new(404, format!("no running machine {id}")))?;
        self.destroy_machine(&m, "destroyed by request", false);
        Ok(())
    }

    /// Wait until no machine is left and no create is in progress, or the
    /// timeout passes. For shutdown, after [`Self::destroy_all`]: a create
    /// that finished late tears its machine down on its own thread.
    pub fn wait_empty(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let empty = self.machines.lock().expect("machines lock").is_empty()
                && self.creating.load(Ordering::SeqCst) == 0;
            if empty || Instant::now() >= deadline {
                return empty;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Refuse new machines and execs, wait for creates in progress, then
    /// destroy every machine. For shutdown.
    pub fn destroy_all(&self, reason: &str) {
        self.stopping.store(true, Ordering::SeqCst);
        // A create that finishes later sees `stopping` and cleans up itself.
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.creating.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let all: Vec<_> = self
            .machines
            .lock()
            .expect("machines lock")
            .values()
            .cloned()
            .collect();
        thread::scope(|s| {
            for m in &all {
                s.spawn(|| self.destroy_machine(m, reason, false));
            }
        });
    }

    /// Stop a machine and remove everything it had. Safe to call twice;
    /// the second call returns at once. Its last event is always
    /// `machine.destroyed`.
    fn destroy_machine(&self, m: &Machine, reason: &str, failed: bool) {
        if self.begin_stop(m, reason) {
            self.teardown(m, reason, failed);
        }
    }

    /// Mark a machine stopping, so it takes no more execs. False if
    /// something else is already stopping it.
    fn begin_stop(&self, m: &Machine, reason: &str) -> bool {
        let mut state = m.state.lock().expect("state lock");
        if *state == State::Stopping {
            return false;
        }
        *state = State::Stopping;
        *m.reason.lock().expect("reason lock") = Some(reason.to_owned());
        true
    }

    /// The rest of a destroy, for whoever won [`Self::begin_stop`].
    fn teardown(&self, m: &Machine, reason: &str, failed: bool) {
        let console = failed.then(|| m.console_tail());
        // The supervisor kills the machine when its lifeline closes.
        drop(m.lifeline.lock().expect("lifeline lock").take());
        let mut child = m.child.lock().expect("child lock");
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
            thread::sleep(Duration::from_millis(20));
        }
        // The final stop needs nothing from the VMM or its supervisor.
        if let Some(g) = &m.cgroup {
            kill_group(g);
        }
        let _ = child.kill();
        let _ = child.wait();
        drop(child);
        // The machine is gone, so an exec in flight ends at once; let it
        // record its end before the machine's.
        let deadline = Instant::now() + EXEC_SETTLE;
        while m.busy.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        // An exec that has not recorded its end by now (its worker never
        // got to run) ends here, before the machine.
        {
            let mut active = m.active.lock().expect("active lock");
            if let Some(exec) = active.take() {
                let _ = self.log.append(
                    &m.id,
                    "exec.failed",
                    json!({"exec": exec, "reason": "machine stopped"}),
                );
            }
        }
        let freed = m.cgroup.as_ref().is_none_or(|g| {
            kill_group(g); // anything that raced in
            remove_group(g)
        });
        let _ = fs::remove_dir_all(&m.dir);
        if freed {
            self.free_slot(m.slot);
        } else {
            eprintln!(
                "sandpiperd: machine {}: processes are left in its cgroup; uid {} is quarantined",
                m.id,
                m.uid.unwrap_or_default()
            );
        }
        let _ = self.finish(&m.id, failed.then_some(reason), reason);
        self.machines.lock().expect("machines lock").remove(&m.id);
        if !freed && let Some(g) = &m.cgroup {
            // Keep the cgroup to try again; the slot stays taken.
            self.quarantine
                .lock()
                .expect("quarantine lock")
                .push((vec![m.slot], g.clone()));
        }
        if let Some(console) = console
            && !console.is_empty()
        {
            eprintln!("sandpiperd: machine {} failed: {reason}\n{console}", m.id);
        }
    }

    /// Record the end of a machine: `failed` first if it failed.
    fn finish(&self, id: &str, failure: Option<&str>, reason: &str) -> io::Result<()> {
        if let Some(f) = failure {
            self.log
                .append(id, "machine.failed", json!({"reason": f}))?;
        }
        self.log
            .append(id, "machine.destroyed", json!({"reason": reason}))?;
        Ok(())
    }

    /// Stop machines whose time is up or whose VMM has exited, and retry
    /// quarantined cgroups.
    fn reap(&self) {
        let all: Vec<_> = self
            .machines
            .lock()
            .expect("machines lock")
            .values()
            .cloned()
            .collect();
        let mut ending = Vec::new();
        for m in all {
            if m.state() == State::Stopping {
                continue;
            }
            let exited = m
                .child
                .lock()
                .expect("child lock")
                .try_wait()
                .ok()
                .flatten();
            let idle = m.state() == State::Running
                && !m.busy.load(Ordering::SeqCst)
                && m.last_active.lock().expect("lock").elapsed() >= m.idle;
            let (reason, failed) = if let Some(status) = exited {
                (format!("VMM exited ({status})"), true)
            } else if m.created.elapsed() >= m.max {
                ("max TTL reached".to_owned(), false)
            } else if idle {
                ("idle TTL reached".to_owned(), false)
            } else {
                continue;
            };
            ending.push((m, reason, failed));
        }
        // Each in its own thread: one slow stop must not hold up the rest.
        thread::scope(|s| {
            for (m, reason, failed) in &ending {
                s.spawn(|| self.destroy_machine(m, reason, *failed));
            }
        });
        let mut q = self.quarantine.lock().expect("quarantine lock");
        q.retain(|(slots, g)| {
            kill_group(g);
            if remove_group(g) {
                for slot in slots {
                    self.free_slot(*slot);
                }
                false
            } else {
                true
            }
        });
    }
}

/// How an exchange ended, short of an error.
enum Ended {
    /// The command exited; the data of its `exec.exited` event.
    Exited(serde_json::Value),
    /// The guest stopped the command at its timeout.
    TimedOut,
    OverQuota,
}

enum Failure {
    /// The relay did not answer at all.
    NoAnswer(io::Error),
    Other(io::Error),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NoAnswer(e) | Failure::Other(e) => e.fmt(f),
        }
    }
}

/// One output stream of an exec, gathered into pieces and cut at the cap.
struct OutputBuffer {
    kind: &'static str,
    pending: Vec<u8>,
    /// When the oldest pending byte came.
    since: Option<Instant>,
    total: usize,
    cut: bool,
}

impl OutputBuffer {
    fn new(kind: &'static str) -> Self {
        Self {
            kind,
            pending: Vec::new(),
            since: None,
            total: 0,
            cut: false,
        }
    }

    fn push(&mut self, b: &[u8]) {
        let room = DEFAULT_OUTPUT_CAP.saturating_sub(self.total);
        let take = b.len().min(room);
        self.cut |= take < b.len();
        if take > 0 {
            self.pending.extend_from_slice(&b[..take]);
            self.since.get_or_insert_with(Instant::now);
        }
        self.total += take;
    }

    /// A full piece, or output that has waited [`FLUSH_EVERY`].
    fn due(&self) -> bool {
        self.pending.len() >= OUTPUT_PIECE || self.since.is_some_and(|t| t.elapsed() >= FLUSH_EVERY)
    }

    fn take(&mut self) -> Option<Vec<u8>> {
        self.since = None;
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

/// Read frames on their own thread, so the reader can wait with a timeout
/// without ever stopping mid-frame.
fn read_frames(mut conn: UnixStream) -> mpsc::Receiver<io::Result<Option<Frame>>> {
    let (tx, rx) = mpsc::sync_channel(16);
    thread::spawn(move || {
        loop {
            let f = read_frame(&mut conn);
            let last = !matches!(f, Ok(Some(_)));
            if tx.send(f).is_err() || last {
                return;
            }
        }
    });
    rx
}

#[cfg(target_os = "linux")]
type CgroupGroup = crate::cgroup::Group;
/// Machines get no cgroup off Linux.
#[cfg(not(target_os = "linux"))]
#[derive(Clone)]
struct CgroupGroup;

#[cfg(target_os = "linux")]
fn kill_group(g: &CgroupGroup) {
    let _ = g.kill();
}

/// Remove an emptied cgroup; false if processes are still in it.
#[cfg(target_os = "linux")]
fn remove_group(g: &CgroupGroup) -> bool {
    !g.populated() && g.remove().is_ok()
}

#[cfg(not(target_os = "linux"))]
fn kill_group(_: &CgroupGroup) {}

#[cfg(not(target_os = "linux"))]
fn remove_group(_: &CgroupGroup) -> bool {
    true
}

struct Decrement<'a>(&'a AtomicUsize);

impl Drop for Decrement<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct ShutdownOnDrop(UnixStream);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// Keep `fd` open across exec in the child, at the same number. Nothing
/// else moves: a `dup2` onto a fixed number could close a descriptor the
/// spawn itself still uses.
fn pass_fd(cmd: &mut Command, fd: i32) {
    use std::os::unix::process::CommandExt;
    // SAFETY: one fcntl call, which is safe between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

fn expect_ready(conn: &mut UnixStream) -> io::Result<()> {
    match read_frame(conn)? {
        Some(Frame::Ready { version }) if version == PROTOCOL_VERSION => Ok(()),
        Some(Frame::Ready { version }) => Err(io::Error::other(format!(
            "guest speaks protocol {version}, host {PROTOCOL_VERSION}"
        ))),
        Some(other) => Err(io::Error::other(format!(
            "expected Ready, got {}",
            other.kind()
        ))),
        None => Err(io::Error::other("relay closed before Ready")),
    }
}

/// Keep the last [`CONSOLE_TAIL`] bytes a VMM prints, for failure reports.
fn capture<R: Read + Send + 'static>(mut src: R, tail: &Arc<Mutex<VecDeque<u8>>>) {
    let tail = Arc::clone(tail);
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n @ 1..) = src.read(&mut buf) {
            let mut t = tail.lock().expect("console lock");
            t.extend(&buf[..n]);
            let excess = t.len().saturating_sub(CONSOLE_TAIL);
            t.drain(..excess);
        }
    });
}

/// Copy a disk image, sharing blocks with the original where the filesystem
/// allows it (reflink).
fn copy_image(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let src = fs::File::open(from)?;
        let dst = fs::File::create(to)?;
        // SAFETY: FICLONE on two open files; failure falls back to a copy.
        if unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONE, src.as_raw_fd()) } == 0 {
            return Ok(());
        }
    }
    fs::copy(from, to).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_buffer_cuts_at_the_cap_and_fills_pieces() {
        let mut b = OutputBuffer::new("exec.stdout");
        b.push(&vec![0; OUTPUT_PIECE - 1]);
        assert!(!b.due());
        b.push(&[1]);
        assert!(b.due());
        assert_eq!(b.take().unwrap().len(), OUTPUT_PIECE);
        assert!(b.take().is_none());
        b.push(&[1]);
        assert!(!b.due());
        thread::sleep(FLUSH_EVERY);
        assert!(b.due(), "a small piece is due once it has waited");
        b.take();
        b.push(&vec![0; DEFAULT_OUTPUT_CAP]);
        assert!(b.cut);
        assert_eq!(b.total, DEFAULT_OUTPUT_CAP);
    }

    #[test]
    fn request_size_counts_everything() {
        let a = ExecArgs {
            cmd: vec!["ab".into(), "c".into()],
            env: vec![("K".into(), "vv".into())],
            cwd: Some("/w".into()),
            stdin: vec![0; 100],
            timeout: None,
        };
        assert_eq!(a.request_size(), r#"[["ab","c"],[["K","vv"]],"/w"]"#.len());
        let many = ExecArgs {
            cmd: vec![String::new(); 1000],
            ..a
        };
        assert!(many.request_size() > 2000);
    }
}
