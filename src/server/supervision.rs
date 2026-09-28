//! Keeps a service manager without cgroup tracking supervising the server
//! across live handoffs.
//!
//! systemd follows a handoff through the service cgroup (`ExitType=cgroup`).
//! launchd only tracks the process it started, and a handoff replaces that
//! process with a detached successor. `herdr server --adopt` closes the gap:
//!
//! - When the supervised process hands its panes off, it re-executes itself as
//!   a small anchor that follows every later handoff and exits only when the
//!   server itself exits: 0 after an intentional stop, non-zero after a crash.
//! - When a server is already running, `--adopt` hands that server's panes to
//!   this executable through a live handoff and anchors the successor instead
//!   of failing with "already running".
//!
//! Two files beside the API socket carry the state between processes:
//! `<socket>.server` records the owning server pid and whether it stopped
//! intentionally, and `<socket>.supervisor` is a POSIX record lock held by the
//! one supervising process for the socket.

use std::path::{Path, PathBuf};

/// Launch flag: supervise across handoffs and adopt a running server.
pub(crate) const ADOPT_FLAG: &str = "--adopt";
/// Hidden re-exec entry point: `server --supervise-anchor <pid> <lock-fd>`.
pub(crate) const ANCHOR_FLAG: &str = "--supervise-anchor";

const SERVER_RECORD_SUFFIX: &str = ".server";
const SUPERVISOR_LOCK_SUFFIX: &str = ".supervisor";

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

pub(crate) fn server_record_path(api_socket: &Path) -> PathBuf {
    with_suffix(api_socket, SERVER_RECORD_SUFFIX)
}

pub(crate) fn supervisor_lock_path(api_socket: &Path) -> PathBuf {
    with_suffix(api_socket, SUPERVISOR_LOCK_SUFFIX)
}

/// The last server that owned the public sockets, and how it left them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerRecord {
    Running(u32),
    Stopped(u32),
}

impl ServerRecord {
    fn render(self) -> String {
        match self {
            Self::Running(pid) => format!("running {pid}\n"),
            Self::Stopped(pid) => format!("stopped {pid}\n"),
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let state = words.next()?;
        let pid = words.next()?.parse::<u32>().ok().filter(|pid| *pid > 0)?;
        if words.next().is_some() {
            return None;
        }
        match state {
            "running" => Some(Self::Running(pid)),
            "stopped" => Some(Self::Stopped(pid)),
            _ => None,
        }
    }

    pub(crate) fn pid(self) -> u32 {
        match self {
            Self::Running(pid) | Self::Stopped(pid) => pid,
        }
    }
}

pub(crate) fn read_server_record(path: &Path) -> Option<ServerRecord> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| ServerRecord::parse(&text))
}

fn write_server_record(path: &Path, record: ServerRecord) -> std::io::Result<()> {
    let temporary = with_suffix(path, &format!(".tmp-{}", std::process::id()));
    let result = std::fs::write(&temporary, record.render())
        .and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Called once this process owns the public sockets: after a cold start binds
/// them, or after a handoff import commits. A successor writes this before its
/// source exits, which is what lets an anchor follow the handoff.
pub(crate) fn record_server_running() {
    let path = server_record_path(&crate::api::socket_path());
    if let Err(err) = write_server_record(&path, ServerRecord::Running(std::process::id())) {
        tracing::warn!(path = %path.display(), err = %err, "failed to record server pid");
    }
}

/// Called when the server exits on purpose (stop request, signal, host
/// shutdown) rather than by crashing or handing off. Only the owner's own
/// record is rewritten, so a stale process never masks its successor.
pub(crate) fn record_server_stopped() {
    let path = server_record_path(&crate::api::socket_path());
    let pid = std::process::id();
    if read_server_record(&path).is_some_and(|record| record.pid() != pid) {
        return;
    }
    if let Err(err) = write_server_record(&path, ServerRecord::Stopped(pid)) {
        tracing::warn!(path = %path.display(), err = %err, "failed to record server stop");
    }
}

/// What the anchor does once the process it watches has exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnchorStep {
    /// A live handoff replaced the watched server; watch its successor.
    Follow(u32),
    /// The server is gone; exit with this status.
    Exit(i32),
}

/// Exit status for a crash whose own status is unknown or zero.
pub(crate) const CRASH_EXIT_CODE: i32 = 1;

pub(crate) fn step_after_exit(
    watched: u32,
    exit_code: Option<i32>,
    record: Option<ServerRecord>,
    alive: impl Fn(u32) -> bool,
) -> AnchorStep {
    match record {
        Some(ServerRecord::Running(successor)) if successor != watched && alive(successor) => {
            AnchorStep::Follow(successor)
        }
        Some(ServerRecord::Stopped(pid)) if pid == watched => AnchorStep::Exit(0),
        _ => AnchorStep::Exit(match exit_code {
            Some(code) if (1..=255).contains(&code) => code,
            _ => CRASH_EXIT_CODE,
        }),
    }
}

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::io::{self, BufRead, BufReader, Write};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use interprocess::local_socket::traits::Stream as _;

    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    const PING_TIMEOUT: Duration = Duration::from_secs(5);
    // The source waits up to 30 s for each import stage; leave room for all.
    const ADOPT_HANDOFF_TIMEOUT: Duration = Duration::from_secs(120);

    /// The exclusive record lock held by the one supervising process.
    ///
    /// POSIX record locks survive `exec` (unlike `flock` on some systems they
    /// also report the holder's pid), but closing any descriptor for the file
    /// in the holding process releases them, so the holder never opens the
    /// lock file anywhere else.
    pub(crate) struct SupervisorLock {
        file: File,
    }

    fn lock_request(kind: libc::c_short) -> libc::flock {
        let mut request: libc::flock = unsafe { std::mem::zeroed() };
        request.l_type = kind;
        request.l_whence = libc::SEEK_SET as libc::c_short;
        request
    }

    impl SupervisorLock {
        /// Takes the lock, or returns `None` when another process holds it.
        pub(crate) fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
            use std::os::unix::fs::OpenOptionsExt;

            // A first start runs before the server has created its socket
            // directory.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)?;
            let lock = Self { file };
            if !lock.relock()? {
                return Ok(None);
            }
            lock.write_owner();
            Ok(Some(lock))
        }

        fn relock(&self) -> io::Result<bool> {
            let request = lock_request(libc::F_WRLCK as libc::c_short);
            if unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_SETLK, &request) } == 0 {
                return Ok(true);
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EACCES) => Ok(false),
                _ => Err(err),
            }
        }

        fn write_owner(&self) {
            let mut file = &self.file;
            let _ = file.set_len(0);
            let _ = file.write_all(format!("{}\n", std::process::id()).as_bytes());
        }

        /// Hands the descriptor over for `exec`. The flag is false when the
        /// descriptor could not be made inheritable, so `exec` would drop it.
        fn into_inheritable_fd(self) -> (RawFd, bool) {
            let fd = self.file.into_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            let inheritable = flags >= 0
                && unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == 0;
            (fd, inheritable)
        }

        /// Rebuilds the lock from a descriptor inherited through `exec`.
        /// POSIX record locks belong to the process and survive `exec`.
        fn from_inherited_fd(fd: RawFd) -> io::Result<Self> {
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let lock = Self {
                file: unsafe { File::from_raw_fd(fd) },
            };
            if !lock.relock()? {
                return Err(io::Error::other(
                    "another process took the supervisor lock during re-exec",
                ));
            }
            lock.write_owner();
            Ok(lock)
        }
    }

    /// The pid holding the supervisor lock, if any.
    pub(crate) fn supervisor_lock_holder(path: &Path) -> io::Result<Option<u32>> {
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let mut request = lock_request(libc::F_WRLCK as libc::c_short);
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut request) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(
            (request.l_type != libc::F_UNLCK as libc::c_short && request.l_pid > 0)
                .then_some(request.l_pid as u32),
        )
    }

    fn process_alive(pid: u32) -> bool {
        crate::platform::process_exists(pid)
    }

    enum Watch {
        Running,
        Exited(Option<i32>),
    }

    /// Reaps the watched process when it is our child (the first successor of
    /// a supervised server is), otherwise probes it with signal 0.
    fn poll_watched(pid: u32) -> Watch {
        let raw = pid as libc::pid_t;
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(raw, &mut status, libc::WNOHANG) };
        if reaped == raw {
            let code = if libc::WIFEXITED(status) {
                Some(libc::WEXITSTATUS(status))
            } else {
                None
            };
            return Watch::Exited(code);
        }
        if reaped == 0 || process_alive(pid) {
            Watch::Running
        } else {
            Watch::Exited(None)
        }
    }

    static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn request_stop(_signal: libc::c_int) {
        STOP_REQUESTED.store(true, Ordering::SeqCst);
    }

    fn install_stop_handlers() {
        let handler: extern "C" fn(libc::c_int) = request_stop;
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            unsafe {
                libc::signal(signal, handler as libc::sighandler_t);
            }
        }
    }

    /// Closes every descriptor except stdio and the supervisor lock, so the
    /// anchor never keeps a pane PTY or socket of its former server alive.
    fn close_inherited_fds(keep: RawFd) {
        let fds: Vec<RawFd> = match std::fs::read_dir("/dev/fd") {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect(),
            Err(_) => (3..1024).collect(),
        };
        for fd in fds {
            if fd > 2 && fd != keep {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }

    fn log(message: impl AsRef<str>) {
        eprintln!(
            "herdr supervisor[{}]: {}",
            std::process::id(),
            message.as_ref()
        );
    }

    /// Follows the server through handoffs until it exits; returns the exit
    /// status the service manager should see.
    fn anchor(mut current: u32, _lock: &SupervisorLock) -> i32 {
        let record_path = server_record_path(&crate::api::socket_path());
        let mut forwarded_stop = false;
        log(format!("supervising herdr server pid {current}"));
        loop {
            if STOP_REQUESTED.load(Ordering::SeqCst) && !forwarded_stop {
                forwarded_stop = true;
                log(format!("stop requested; stopping server pid {current}"));
                unsafe {
                    libc::kill(current as libc::pid_t, libc::SIGTERM);
                }
            }
            match poll_watched(current) {
                Watch::Running => std::thread::sleep(POLL_INTERVAL),
                Watch::Exited(code) => {
                    let record = read_server_record(&record_path);
                    match step_after_exit(current, code, record, process_alive) {
                        AnchorStep::Follow(successor) => {
                            log(format!(
                                "server pid {current} handed off to pid {successor}"
                            ));
                            current = successor;
                        }
                        AnchorStep::Exit(status) => {
                            if status == 0 {
                                log(format!("server pid {current} stopped"));
                            } else {
                                log(format!(
                                    "server pid {current} exited without stopping or handing off; exiting {status}"
                                ));
                            }
                            return status;
                        }
                    }
                }
            }
        }
    }

    fn successor_command(program: PathBuf, successor: u32, fd: RawFd) -> std::process::Command {
        let mut command = std::process::Command::new(program);
        command
            .arg("server")
            .arg(ANCHOR_FLAG)
            .arg(successor.to_string())
            .arg(fd.to_string());
        if crate::session::explicit_session_requested() {
            // Like a handoff import, the anchor finds its session through the
            // inherited HERDR_SESSION rather than the original argument.
            command
                .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
                .env_remove(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR);
        }
        command
    }

    /// Turns this process, which launchd tracks, into the anchor for
    /// `successor`. Re-executing drops the old server's memory, threads, and
    /// descriptors while keeping the pid. Never returns.
    pub(crate) fn become_anchor(successor: u32, lock: SupervisorLock) -> ! {
        let (fd, inheritable) = lock.into_inheritable_fd();
        if inheritable {
            match crate::platform::reexec_path() {
                Ok(program) => {
                    let err = successor_command(program, successor, fd).exec();
                    log(format!(
                        "could not re-exec as anchor ({err}); anchoring in place"
                    ));
                }
                Err(err) => log(format!(
                    "cannot locate this executable to re-exec ({err}); anchoring in place"
                )),
            }
        }
        install_stop_handlers();
        match SupervisorLock::from_inherited_fd(fd) {
            Ok(lock) => std::process::exit(anchor(successor, &lock)),
            Err(err) => {
                log(format!("lost the supervisor lock: {err}"));
                std::process::exit(CRASH_EXIT_CODE);
            }
        }
    }

    /// Entry point for `server --supervise-anchor <pid> <lock-fd>`.
    pub(crate) fn run_anchor_command(args: &[String]) -> ! {
        let parsed = match args {
            [pid, fd] => pid.parse::<u32>().ok().zip(fd.parse::<RawFd>().ok()),
            _ => None,
        };
        let Some((pid, fd)) = parsed else {
            log("usage: herdr server --supervise-anchor <pid> <lock-fd>");
            std::process::exit(2);
        };
        close_inherited_fds(fd);
        install_stop_handlers();
        let lock = match SupervisorLock::from_inherited_fd(fd) {
            Ok(lock) => lock,
            Err(err) => {
                log(format!("cannot keep the supervisor lock: {err}"));
                std::process::exit(CRASH_EXIT_CODE);
            }
        };
        std::process::exit(anchor(pid, &lock));
    }

    /// Takes the supervisor lock for `--adopt`, or exits 0 (which launchd
    /// does not restart) when another process already supervises the socket.
    pub(crate) fn acquire_or_exit() -> SupervisorLock {
        let path = supervisor_lock_path(&crate::api::socket_path());
        match SupervisorLock::try_acquire(&path) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                let holder = supervisor_lock_holder(&path)
                    .ok()
                    .flatten()
                    .map_or_else(|| "another process".to_string(), |pid| format!("pid {pid}"));
                log(format!(
                    "herdr server is already supervised by {holder}; nothing to do"
                ));
                std::process::exit(0);
            }
            Err(err) => {
                log(format!(
                    "cannot open supervisor lock {}: {err}",
                    path.display()
                ));
                std::process::exit(CRASH_EXIT_CODE);
            }
        }
    }

    fn api_request(
        socket: &Path,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let mut stream = crate::ipc::connect_local_stream(socket)
            .map_err(|err| format!("cannot connect to {}: {err}", socket.display()))?;
        stream
            .set_recv_timeout(Some(timeout))
            .and_then(|()| stream.set_send_timeout(Some(timeout)))
            .map_err(|err| format!("cannot set socket timeout: {err}"))?;
        let request = serde_json::json!({
            "id": format!("supervisor:{method}"),
            "method": method,
            "params": params,
        });
        stream
            .write_all(format!("{request}\n").as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|err| format!("cannot send {method}: {err}"))?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(|err| format!("no {method} response: {err}"))?;
        let response: serde_json::Value = serde_json::from_str(&line)
            .map_err(|err| format!("invalid {method} response {line:?}: {err}"))?;
        if let Some(error) = response.get("error") {
            return Err(format!("{method} failed: {error}"));
        }
        Ok(response)
    }

    /// Hands a running, unsupervised server's panes to this executable and
    /// anchors the successor. Adoption never retries: a failure leaves the
    /// running server untouched and exits 0 so launchd does not repeat a
    /// disruptive handoff in a loop. Never returns.
    pub(crate) fn adopt_running_server(lock: SupervisorLock) -> ! {
        let socket = crate::api::socket_path();
        let record_path = server_record_path(&socket);
        let refuse = |reason: String| -> ! {
            log(format!(
                "not adopting the running server: {reason}; it keeps running unsupervised"
            ));
            std::process::exit(0);
        };

        let pong = api_request(&socket, "ping", serde_json::json!({}), PING_TIMEOUT)
            .unwrap_or_else(|err| refuse(err));
        let supports_handoff = pong["result"]["capabilities"]["live_handoff"]
            .as_bool()
            .unwrap_or(false);
        if !supports_handoff {
            refuse(format!(
                "server {} does not support live handoff",
                pong["result"]["version"]
                    .as_str()
                    .unwrap_or("of unknown version")
            ));
        }
        let exe = std::env::current_exe()
            .unwrap_or_else(|err| refuse(format!("cannot resolve this executable: {err}")));
        log(format!(
            "adopting running server {} through a live handoff to {}",
            pong["result"]["version"].as_str().unwrap_or("?"),
            exe.display()
        ));
        api_request(
            &socket,
            "server.live_handoff",
            serde_json::json!({
                "import_exe": exe.display().to_string(),
                "expected_protocol": crate::protocol::PROTOCOL_VERSION,
                "expected_version": crate::build_info::version(),
            }),
            ADOPT_HANDOFF_TIMEOUT,
        )
        .unwrap_or_else(|err| refuse(err));

        match read_server_record(&record_path) {
            Some(ServerRecord::Running(pid)) if pid != std::process::id() && process_alive(pid) => {
                log(format!("adopted server pid {pid}"));
                become_anchor(pid, lock)
            }
            other => refuse(format!(
                "handoff succeeded but {} names no live successor ({other:?})",
                record_path.display()
            )),
        }
    }

    /// JSON for `herdr server supervision`, which also tells service
    /// installers that this build supports `--adopt`.
    pub(crate) fn supervision_status() -> serde_json::Value {
        let socket = crate::api::socket_path();
        let server = read_server_record(&server_record_path(&socket)).map(|record| {
            let (state, pid) = match record {
                ServerRecord::Running(pid) => ("running", pid),
                ServerRecord::Stopped(pid) => ("stopped", pid),
            };
            serde_json::json!({"pid": pid, "state": state, "alive": process_alive(pid)})
        });
        let supervisor = supervisor_lock_holder(&supervisor_lock_path(&socket))
            .ok()
            .flatten();
        serde_json::json!({
            "adopt": true,
            "api_socket": socket.display().to_string(),
            "server": server,
            "supervisor_pid": supervisor,
        })
    }
}

#[cfg(not(unix))]
pub(crate) fn supervision_status() -> serde_json::Value {
    serde_json::json!({"adopt": false})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_reject_garbage() {
        for record in [ServerRecord::Running(42), ServerRecord::Stopped(7)] {
            assert_eq!(ServerRecord::parse(&record.render()), Some(record));
        }
        for text in [
            "",
            "running",
            "running 0",
            "running x",
            "paused 3",
            "running 3 4",
        ] {
            assert_eq!(ServerRecord::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn state_files_sit_beside_the_api_socket() {
        let socket = Path::new("/run/user/1/herdr/herdr.sock");
        assert_eq!(
            server_record_path(socket),
            Path::new("/run/user/1/herdr/herdr.sock.server")
        );
        assert_eq!(
            supervisor_lock_path(socket),
            Path::new("/run/user/1/herdr/herdr.sock.supervisor")
        );
    }

    #[test]
    fn anchor_follows_a_live_successor() {
        let step = step_after_exit(10, Some(0), Some(ServerRecord::Running(11)), |_| true);
        assert_eq!(step, AnchorStep::Follow(11));
    }

    #[test]
    fn anchor_exits_zero_after_an_intentional_stop() {
        let step = step_after_exit(10, None, Some(ServerRecord::Stopped(10)), |_| false);
        assert_eq!(step, AnchorStep::Exit(0));
    }

    #[test]
    fn anchor_reports_a_crash_as_failure() {
        // The record still names the dead server: it neither stopped nor handed off.
        assert_eq!(
            step_after_exit(10, None, Some(ServerRecord::Running(10)), |_| false),
            AnchorStep::Exit(CRASH_EXIT_CODE)
        );
        // A known non-zero status is passed on.
        assert_eq!(
            step_after_exit(10, Some(101), Some(ServerRecord::Running(10)), |_| false),
            AnchorStep::Exit(101)
        );
        // A clean status without a stop record is still a failure to launchd.
        assert_eq!(
            step_after_exit(10, Some(0), None, |_| false),
            AnchorStep::Exit(CRASH_EXIT_CODE)
        );
        // A successor that already died is not followed.
        assert_eq!(
            step_after_exit(10, Some(0), Some(ServerRecord::Running(11)), |_| false),
            AnchorStep::Exit(CRASH_EXIT_CODE)
        );
        // Another server's stop record does not describe the watched one.
        assert_eq!(
            step_after_exit(10, None, Some(ServerRecord::Stopped(12)), |_| false),
            AnchorStep::Exit(CRASH_EXIT_CODE)
        );
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_lock_is_exclusive_and_reports_its_holder() {
        use std::os::unix::ffi::OsStrExt;

        let dir = std::env::temp_dir().join(format!("herdr-supervision-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The socket directory may not exist yet on a first start.
        let path = dir.join("not-yet-created").join("herdr.sock.supervisor");
        assert_eq!(supervisor_lock_holder(&path).unwrap(), None);

        let held = SupervisorLock::try_acquire(&path).unwrap().expect("lock");

        // Record locks never conflict within one process, so probe from a
        // forked child that only makes async-signal-safe calls. (Reading the
        // file here would itself drop the lock: closing any descriptor for a
        // file releases the process's record locks on it.)
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let parent = std::process::id() as libc::pid_t;
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            let code = unsafe {
                let fd = libc::open(c_path.as_ptr(), libc::O_RDWR);
                let mut probe: libc::flock = std::mem::zeroed();
                probe.l_type = libc::F_WRLCK as libc::c_short;
                probe.l_whence = libc::SEEK_SET as libc::c_short;
                let mut query = probe;
                if fd < 0 || libc::fcntl(fd, libc::F_SETLK, &probe) == 0 {
                    1
                } else if libc::fcntl(fd, libc::F_GETLK, &mut query) != 0 || query.l_pid != parent {
                    2
                } else {
                    0
                }
            };
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "another process saw the lock as free or not owned by the holder"
        );

        drop(held);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        assert_eq!(supervisor_lock_holder(&path).unwrap(), None);
        assert!(SupervisorLock::try_acquire(&path).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn server_record_writes_replace_atomically() {
        let dir = std::env::temp_dir().join(format!("herdr-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("herdr.sock.server");
        write_server_record(&path, ServerRecord::Running(5)).unwrap();
        assert_eq!(read_server_record(&path), Some(ServerRecord::Running(5)));
        write_server_record(&path, ServerRecord::Stopped(5)).unwrap();
        assert_eq!(read_server_record(&path), Some(ServerRecord::Stopped(5)));
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
