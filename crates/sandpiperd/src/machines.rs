//! Machines on this host (DESIGN.md §4.2, §5): create, exec and destroy,
//! with time limits. Everything here blocks; the API calls it from blocking
//! tasks.
//!
//! Each machine is one `sandpiper-vmm boot` process. On a Linux host it is
//! sandboxed: its own uid from [`UID_BASE`], a jail, its own cgroup. The
//! host agent never reads the guest's raw frames: it talks to the relay in
//! the VMM process (see `sandpiper-vmm`'s `relay`), and checks that the
//! relay's socket belongs to the machine's uid.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sandpiper_proto::{ExecRequest, Frame, PROTOCOL_VERSION, read_frame, write_frame};
use serde_json::json;

use crate::events::{Log, now_ms};
use crate::ids;

/// Per-machine uids and gids are `UID_BASE + slot`: far above login users,
/// the usual /etc/subuid ranges, and the ids `sandpiper-vmm run` uses.
pub const UID_BASE: u32 = 1_900_000_000;

const BOOT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a machine gets to stop after its lifeline closes, before
/// `cgroup.kill`.
const STOP_GRACE: Duration = Duration::from_secs(2);
const CONSOLE_TAIL: usize = 16 * 1024;
const STDIN_CHUNK: usize = 32 * 1024;

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
    Running,
    Stopping,
}

struct Machine {
    id: String,
    template: String,
    uid: Option<u32>,
    dir: PathBuf,
    created: Instant,
    created_ms: u64,
    idle: Duration,
    max: Duration,
    child: Mutex<Child>,
    /// Closing it tells the VMM's supervisor to kill the machine.
    lifeline: Mutex<Option<io::PipeWriter>>,
    #[cfg(target_os = "linux")]
    cgroup: Option<crate::cgroup::Group>,
    state: Mutex<State>,
    /// Why the machine is stopping, once it is.
    reason: Mutex<Option<String>>,
    last_active: Mutex<Instant>,
    busy: AtomicBool,
    execs: AtomicU64,
    console: Arc<Mutex<VecDeque<u8>>>,
}

impl Machine {
    fn relay(&self) -> PathBuf {
        self.dir.join("machine").join("host.sock")
    }

    fn info(&self) -> serde_json::Value {
        let state = match *self.state.lock().expect("state lock") {
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
}

/// Spawns VMM processes from one thread that never exits. A sandboxed
/// VMM's supervisor dies with the *thread* that spawned it
/// (`PR_SET_PDEATHSIG`), so short-lived worker threads cannot do it.
struct Spawner(Mutex<mpsc::Sender<(Command, mpsc::Sender<io::Result<Child>>)>>);

impl Spawner {
    fn start() -> Self {
        let (tx, rx) = mpsc::channel::<(Command, mpsc::Sender<io::Result<Child>>)>();
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
    uids: Mutex<BTreeSet<u32>>,
    spawner: Spawner,
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
        #[cfg(target_os = "linux")]
        let cgroups = if cfg.sandbox {
            let root = crate::cgroup::Root::setup(&cfg.state.join("machines"))?;
            root.remove_leftovers();
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
            uids: Mutex::new(BTreeSet::new()),
            spawner: Spawner::start(),
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
        let id = ids::new_id("m");
        let uid = if self.cfg.sandbox {
            Some(self.alloc_uid()?)
        } else {
            if self.machines.lock().expect("machines lock").len() >= self.cfg.max_machines {
                return Err(Error::new(503, "this host is full"));
            }
            None
        };
        self.log.append(
            &id,
            "machine.created",
            json!({"template": req.template, "metadata": req.metadata}),
        )?;
        let started = Instant::now();
        let dir = self.cfg.state.join("machines").join(&id);
        let launched = self.launch(&id, &dir, &image, uid);
        let (child, lifeline, console, cgroup) = match launched {
            Ok(parts) => parts,
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                self.free_uid(uid);
                self.finish(&id, Some(&e.to_string()), "failed to start")?;
                return Err(Error::new(500, format!("machine failed to start: {e}")));
            }
        };
        let machine = Arc::new(Machine {
            id: id.clone(),
            template: req.template,
            uid,
            dir,
            created: started,
            created_ms: now_ms(),
            idle,
            max,
            child: Mutex::new(child),
            lifeline: Mutex::new(Some(lifeline)),
            #[cfg(target_os = "linux")]
            cgroup,
            state: Mutex::new(State::Running),
            reason: Mutex::new(None),
            last_active: Mutex::new(Instant::now()),
            busy: AtomicBool::new(true), // until it is ready
            execs: AtomicU64::new(0),
            console,
        });
        #[cfg(not(target_os = "linux"))]
        let _ = cgroup;
        self.machines
            .lock()
            .expect("machines lock")
            .insert(id.clone(), Arc::clone(&machine));
        match self.wait_ready(&machine) {
            Ok(()) => {
                *machine.last_active.lock().expect("lock") = Instant::now();
                machine.busy.store(false, Ordering::SeqCst);
                self.log.append(
                    &id,
                    "machine.running",
                    json!({"readyMs": started.elapsed().as_millis() as u64}),
                )?;
                Ok(machine.info())
            }
            Err(e) => {
                let reason = format!("machine failed to start: {e}");
                self.destroy_machine(&machine, &reason, true);
                Err(Error::new(500, reason))
            }
        }
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

    fn alloc_uid(&self) -> Result<u32> {
        let mut uids = self.uids.lock().expect("uids lock");
        if uids.len() >= self.cfg.max_machines {
            return Err(Error::new(503, "this host is full"));
        }
        let slot = (0..u32::MAX)
            .find(|s| !uids.contains(&(UID_BASE + s)))
            .expect("fewer machines than uids");
        uids.insert(UID_BASE + slot);
        Ok(UID_BASE + slot)
    }

    fn free_uid(&self, uid: Option<u32>) {
        if let Some(uid) = uid {
            self.uids.lock().expect("uids lock").remove(&uid);
        }
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
        let machine = dir.join("machine");
        fs::DirBuilder::new().mode(0o700).create(dir)?;
        fs::DirBuilder::new().mode(0o700).create(&machine)?;
        let disk = machine.join("root.img");
        copy_image(image, &disk)?;

        let mut cmd = Command::new(&self.cfg.vmm);
        cmd.arg("boot")
            .arg("--disk")
            .arg(&disk)
            .arg("--socket")
            .arg(machine.join("agent.sock"))
            .args(["--cpus", &self.cfg.cpus.to_string()])
            .args(["--mem", &self.cfg.mem_mib.to_string()])
            .arg("--die-with-parent");
        let cgroup = match uid {
            Some(uid) => {
                chown_tree(&machine, uid)?;
                cmd.arg("--jail").arg(dir).args([
                    "--uid",
                    &uid.to_string(),
                    "--gid",
                    &uid.to_string(),
                ]);
                self.make_cgroup(id, &mut cmd)?
            }
            None => {
                cmd.env("SANDPIPER_UNSANDBOXED", "1");
                None
            }
        };
        let (lifeline_r, lifeline_w) = io::pipe()?;
        cmd.stdin(lifeline_r)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match self.spawner.spawn(cmd) {
            Ok(c) => c,
            Err(e) => {
                remove_cgroup(&cgroup);
                return Err(e);
            }
        };
        let console = Arc::new(Mutex::new(VecDeque::new()));
        capture(child.stdout.take().expect("piped"), &console);
        capture(child.stderr.take().expect("piped"), &console);
        Ok((child, lifeline_w, console, cgroup))
    }

    #[cfg(target_os = "linux")]
    fn make_cgroup(&self, id: &str, cmd: &mut Command) -> io::Result<Option<CgroupGroup>> {
        use std::os::fd::AsRawFd;
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
        let deadline = Instant::now() + BOOT_TIMEOUT;
        let mut conn = loop {
            if let Some(status) = m.child.lock().expect("child lock").try_wait()? {
                return Err(io::Error::other(format!(
                    "VMM exited during boot ({status})"
                )));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("timed out waiting for the guest agent"));
            }
            match self.connect_relay(m) {
                Ok(c) => break c,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        };
        conn.set_read_timeout(Some(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
        ))?;
        expect_ready(&mut conn)
    }

    /// Connect to the machine's relay, refusing any socket not owned by
    /// the machine's uid: its directory belongs to that uid, so the path
    /// could have been swapped.
    fn connect_relay(&self, m: &Machine) -> io::Result<UnixStream> {
        let conn = UnixStream::connect(m.relay())?;
        if let Some(want) = m.uid {
            let got = peer_uid(&conn)?;
            if got != want {
                return Err(io::Error::other(format!(
                    "relay socket peer is uid {got}, not the machine's uid {want}"
                )));
            }
        }
        Ok(conn)
    }

    /// A running machine.
    fn get(&self, id: &str) -> Result<Arc<Machine>> {
        self.machines
            .lock()
            .expect("machines lock")
            .get(id)
            .filter(|m| *m.state.lock().expect("state lock") == State::Running)
            .cloned()
            .ok_or_else(|| Error::new(404, format!("no running machine {id}")))
    }

    /// A machine's current info, or its last known state from the log.
    pub fn info(&self, id: &str) -> Result<serde_json::Value> {
        if let Some(m) = self.machines.lock().expect("machines lock").get(id) {
            return Ok(m.info());
        }
        let Some(last) = self.log.last_of(id, "machine.")? else {
            return Err(Error::new(404, format!("no machine {id}")));
        };
        let created = self.log.read(id, 1, 1)?.pop();
        let failed = self
            .log
            .read(id, 1, 1_000_000)?
            .iter()
            .any(|e| e.kind == "machine.failed");
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
    /// number of its first event. The exec itself runs in [`run_exec`].
    pub fn begin_exec(&self, id: &str, args: &ExecArgs) -> Result<(String, u64)> {
        let m = self.get(id)?;
        if args.cmd.is_empty() {
            return Err(Error::new(400, "cmd must not be empty"));
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
        if m.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::new(
                409,
                "an exec is already running on this machine",
            ));
        }
        *m.last_active.lock().expect("lock") = Instant::now();
        let exec = format!("e{}", m.execs.fetch_add(1, Ordering::SeqCst) + 1);
        match self
            .log
            .append(id, "exec.started", json!({"exec": exec, "cmd": args.cmd}))
        {
            Ok(e) => Ok((exec, e.seq)),
            Err(e) => {
                m.busy.store(false, Ordering::SeqCst);
                Err(e.into())
            }
        }
    }

    /// Run an exec that [`begin_exec`] recorded. Its output and result go
    /// to the event log; this returns when it has ended.
    pub fn run_exec(&self, id: &str, exec: &str, args: ExecArgs) {
        let Ok(m) = self.get(id) else {
            let _ = self.log.append(
                id,
                "exec.failed",
                json!({"exec": exec, "reason": "machine stopped"}),
            );
            return;
        };
        let timeout = args.timeout.unwrap_or(self.cfg.exec_max);
        let timed_out = Arc::new(AtomicBool::new(false));
        let result = self.exchange(&m, exec, args, timeout, &timed_out);
        let end = match result {
            Ok(()) => None,
            Err(_) if timed_out.load(Ordering::SeqCst) => Some((
                "exec.timedout",
                json!({"exec": exec, "timeoutMs": timeout.as_millis() as u64}),
            )),
            Err(e) => {
                let stopping = *m.state.lock().expect("state lock") != State::Running;
                let reason = if stopping {
                    "machine stopped".to_owned()
                } else {
                    e.to_string()
                };
                Some(("exec.failed", json!({"exec": exec, "reason": reason})))
            }
        };
        if let Some((kind, data)) = end {
            let _ = self.log.append(id, kind, data);
        }
        *m.last_active.lock().expect("lock") = Instant::now();
        m.busy.store(false, Ordering::SeqCst);
    }

    fn exchange(
        &self,
        m: &Machine,
        exec: &str,
        args: ExecArgs,
        timeout: Duration,
        timed_out: &Arc<AtomicBool>,
    ) -> io::Result<()> {
        let mut conn = self.connect_relay(m)?;
        conn.set_read_timeout(Some(BOOT_TIMEOUT))?;
        expect_ready(&mut conn)?;
        conn.set_read_timeout(None)?;

        // Hanging up ends the exchange whatever it is blocked on; the guest
        // then kills the command's process group.
        let done = Arc::new(AtomicBool::new(false));
        {
            let (conn, done, timed_out) =
                (conn.try_clone()?, Arc::clone(&done), Arc::clone(timed_out));
            let deadline = Instant::now() + timeout;
            thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    if Instant::now() >= deadline {
                        timed_out.store(true, Ordering::SeqCst);
                        let _ = conn.shutdown(Shutdown::Both);
                        return;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
        }
        let _end = SetOnDrop(Arc::clone(&done));

        write_frame(
            &mut conn,
            &Frame::Exec(ExecRequest {
                argv: args.cmd,
                env: args.env,
                cwd: args.cwd,
            }),
        )?;
        // Fed from its own thread so output is read while input is sent.
        let mut feeder = conn.try_clone()?;
        let stdin = args.stdin;
        thread::spawn(move || -> io::Result<()> {
            for chunk in stdin.chunks(STDIN_CHUNK) {
                write_frame(&mut feeder, &Frame::Stdin(chunk.to_vec()))?;
            }
            write_frame(&mut feeder, &Frame::Stdin(Vec::new()))
        });

        let id = &m.id;
        loop {
            match read_frame(&mut conn)? {
                Some(Frame::Stdout(b)) => {
                    self.log.append(
                        id,
                        "exec.stdout",
                        json!({"exec": exec, "data": B64.encode(b)}),
                    )?;
                }
                Some(Frame::Stderr(b)) => {
                    self.log.append(
                        id,
                        "exec.stderr",
                        json!({"exec": exec, "data": B64.encode(b)}),
                    )?;
                }
                Some(Frame::Error(msg)) => {
                    self.log.append(
                        id,
                        "exec.error",
                        json!({"exec": exec, "message": crate::sanitize(&msg, 4096)}),
                    )?;
                }
                Some(Frame::Exit(s)) => {
                    self.log.append(
                        id,
                        "exec.exited",
                        json!({
                            "exec": exec,
                            "code": s.code,
                            "stdoutTruncated": s.stdout_truncated,
                            "stderrTruncated": s.stderr_truncated,
                        }),
                    )?;
                    return Ok(());
                }
                Some(other) => {
                    return Err(io::Error::other(format!(
                        "unexpected {} frame",
                        other.kind()
                    )));
                }
                None => return Err(io::Error::other("the machine closed the connection")),
            }
        }
    }

    pub fn destroy(&self, id: &str) -> Result<()> {
        let m = self
            .machines
            .lock()
            .expect("machines lock")
            .get(id)
            .cloned()
            .ok_or_else(|| Error::new(404, format!("no running machine {id}")))?;
        self.destroy_machine(&m, "destroyed by request", false);
        Ok(())
    }

    /// Destroy every machine, for shutdown.
    pub fn destroy_all(&self, reason: &str) {
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
    /// the second call returns at once.
    fn destroy_machine(&self, m: &Machine, reason: &str, failed: bool) {
        {
            let mut state = m.state.lock().expect("state lock");
            if *state == State::Stopping {
                return;
            }
            *state = State::Stopping;
        }
        *m.reason.lock().expect("reason lock") = Some(reason.to_owned());
        let console = failed.then(|| m.console_tail());
        // The supervisor kills the machine when its lifeline closes.
        drop(m.lifeline.lock().expect("lifeline lock").take());
        let mut child = m.child.lock().expect("child lock");
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
            thread::sleep(Duration::from_millis(20));
        }
        // The final stop needs nothing from the VMM or its supervisor.
        #[cfg(target_os = "linux")]
        if let Some(g) = &m.cgroup {
            let _ = g.kill();
        }
        let _ = child.kill();
        let _ = child.wait();
        drop(child);
        #[cfg(target_os = "linux")]
        if let Some(g) = &m.cgroup {
            let _ = g.kill(); // anything that raced in
            if g.populated() {
                eprintln!("sandpiperd: machine {} cgroup still populated", m.id);
            }
            let _ = g.remove();
        }
        let _ = fs::remove_dir_all(&m.dir);
        self.free_uid(m.uid);
        let _ = self.finish(&m.id, failed.then_some(reason), reason);
        self.machines.lock().expect("machines lock").remove(&m.id);
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

    /// Stop machines whose time is up or whose VMM has exited.
    fn reap(&self) {
        let all: Vec<_> = self
            .machines
            .lock()
            .expect("machines lock")
            .values()
            .cloned()
            .collect();
        for m in all {
            if *m.state.lock().expect("state lock") != State::Running {
                continue;
            }
            let exited = m
                .child
                .lock()
                .expect("child lock")
                .try_wait()
                .ok()
                .flatten();
            let idle = !m.busy.load(Ordering::SeqCst)
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
            self.destroy_machine(&m, &reason, failed);
        }
    }
}

#[cfg(target_os = "linux")]
type CgroupGroup = crate::cgroup::Group;
#[cfg(not(target_os = "linux"))]
type CgroupGroup = ();

#[cfg(target_os = "linux")]
fn remove_cgroup(g: &Option<CgroupGroup>) {
    if let Some(g) = g {
        let _ = g.remove();
    }
}

#[cfg(not(target_os = "linux"))]
fn remove_cgroup(_: &Option<CgroupGroup>) {}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
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
        use std::os::fd::AsRawFd;
        let src = fs::File::open(from)?;
        let dst = fs::File::create(to)?;
        // SAFETY: FICLONE on two open files; failure falls back to a copy.
        if unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONE, src.as_raw_fd()) } == 0 {
            return Ok(());
        }
    }
    fs::copy(from, to).map(drop)
}

fn chown_tree(dir: &Path, id: u32) -> io::Result<()> {
    std::os::unix::fs::chown(dir, Some(id), Some(id))?;
    for entry in fs::read_dir(dir)? {
        std::os::unix::fs::chown(entry?.path(), Some(id), Some(id))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn peer_uid(conn: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt into a correctly sized ucred.
    let rc = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(_: &UnixStream) -> io::Result<u32> {
    Err(io::Error::other(
        "peer credentials are checked on Linux only",
    ))
}
