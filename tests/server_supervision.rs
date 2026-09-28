#![cfg(unix)]

//! `herdr server --adopt`: supervision that survives live handoffs under a
//! service manager without cgroup tracking (launchd).

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid, wait_for_socket,
};

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Env {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    api_socket: PathBuf,
}

impl Env {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = PathBuf::from(format!("/tmp/hsv-{}-{n}", std::process::id()));
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        for app_dir in ["herdr", "herdr-dev"] {
            fs::create_dir_all(config_home.join(app_dir)).unwrap();
            fs::write(
                config_home.join(app_dir).join("config.toml"),
                "onboarding = false\n",
            )
            .unwrap();
        }
        fs::create_dir_all(&runtime_dir).unwrap();
        register_runtime_dir(&runtime_dir);
        let api_socket = runtime_dir.join("herdr.sock");
        Self {
            base,
            config_home,
            runtime_dir,
            api_socket,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command.args(args);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(key);
            }
        }
        command
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("XDG_STATE_HOME", self.base.join("state"))
            .env("HERDR_SOCKET_PATH", &self.api_socket)
            .env(
                "HERDR_CLIENT_SOCKET_PATH",
                self.runtime_dir.join("herdr-client.sock"),
            )
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null());
        command
    }

    fn spawn(&self, args: &[&str], log: &str) -> Child {
        let log = fs::File::create(self.base.join(log)).unwrap();
        let child = self
            .command(args)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        register_spawned_herdr_pid(Some(child.id()));
        child
    }

    fn record(&self) -> String {
        fs::read_to_string(self.runtime_dir.join("herdr.sock.server"))
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn running_pid(&self) -> u32 {
        let record = self.record();
        record
            .strip_prefix("running ")
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("server record is not running: {record:?}"))
    }

    fn request(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let mut stream = UnixStream::connect(&self.api_socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let request = serde_json::json!({"id": "test", "method": method, "params": params});
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(
            response.get("result").is_some(),
            "{method} failed: {response}"
        );
        response
    }

    fn handoff(&self) {
        self.request("server.live_handoff", serde_json::json!({}));
    }

    /// Starts a long-running process in a new pane and returns its pid.
    fn start_pane_process(&self) -> u32 {
        let created = self.request(
            "workspace.create",
            serde_json::json!({"cwd": "/tmp", "focus": true}),
        );
        let pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        let marker = self.base.join("pane.pid");
        let command = format!("sh -c 'echo $$ > {}; exec sleep 600'", marker.display());
        self.request(
            "pane.send_input",
            serde_json::json!({"pane_id": pane_id, "text": command, "keys": ["Enter"]}),
        );
        wait_until("pane process marker", || {
            fs::read_to_string(&marker).is_ok_and(|text| text.ends_with('\n'))
        });
        fs::read_to_string(&marker).unwrap().trim().parse().unwrap()
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.base.join(name)).unwrap_or_default()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // The runtime-dir watchdog in `support` only works on Linux, and a
        // handed-off server is nobody's child, so stop it through its socket.
        if let Ok(mut stream) = UnixStream::connect(&self.api_socket) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream
                .write_all(b"{\"id\":\"cleanup\",\"method\":\"server.stop\",\"params\":{}}\n");
            let mut line = String::new();
            let _ = BufReader::new(stream).read_line(&mut line);
        }
        cleanup_test_base(&self.base);
    }
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

fn wait_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    panic!("process {} did not exit", child.id());
}

fn still_running(child: &mut Child) -> bool {
    child.try_wait().unwrap().is_none()
}

fn wait_record_changes_from(env: &Env, previous: u32) -> u32 {
    wait_until("successor record", || {
        env.record()
            .strip_prefix("running ")
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_some_and(|pid| pid != previous)
    });
    env.running_pid()
}

#[test]
fn adopt_mode_anchor_follows_handoffs_and_exits_zero_on_stop() {
    let _lock = test_lock();
    let env = Env::new();
    let mut anchor = env.spawn(&["server", "--adopt"], "anchor.log");
    wait_for_socket(&env.api_socket, Duration::from_secs(10));
    wait_until("server record", || {
        env.record() == format!("running {}", anchor.id())
    });
    let pane_process = env.start_pane_process();

    let mut previous = anchor.id();
    for hop in 0..2 {
        env.handoff();
        let current = wait_record_changes_from(&env, previous);
        assert_ne!(current, anchor.id());
        assert!(
            still_running(&mut anchor),
            "the launchd-tracked process must survive handoff {hop}: {}",
            env.log("anchor.log")
        );
        assert!(alive(pane_process), "pane process died in handoff {hop}");
        previous = current;
    }
    wait_until("anchor to follow the second handoff", || {
        env.log("anchor.log")
            .contains(&format!("handed off to pid {previous}"))
    });

    // The supervised socket already has a supervisor, so a second instance
    // must neither hand off nor exit with a status launchd would restart.
    let second = env.command(&["server", "--adopt"]).output().unwrap();
    assert!(second.status.success(), "{second:?}");
    assert!(String::from_utf8_lossy(&second.stderr).contains("already supervised"));
    assert_eq!(env.running_pid(), previous);

    let status = env.command(&["server", "supervision"]).output().unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["adopt"], true);
    assert_eq!(status["supervisor_pid"], anchor.id());
    assert_eq!(status["server"]["pid"], previous);
    assert_eq!(status["server"]["state"], "running");

    env.request("server.stop", serde_json::json!({}));
    let status = wait_exit(&mut anchor);
    assert_eq!(status.code(), Some(0), "{}", env.log("anchor.log"));
    assert_eq!(env.record(), format!("stopped {previous}"));
}

#[test]
fn adopt_takes_over_a_running_server_and_sigterm_stops_it() {
    let _lock = test_lock();
    let env = Env::new();
    let mut plain = env.spawn(&["server"], "plain.log");
    wait_for_socket(&env.api_socket, Duration::from_secs(10));
    let pane_process = env.start_pane_process();
    // Behave like a server built before supervision records existed.
    fs::remove_file(env.runtime_dir.join("herdr.sock.server")).unwrap();

    let mut anchor = env.spawn(&["server", "--adopt"], "anchor.log");
    wait_until("adoption", || {
        env.log("anchor.log")
            .contains("supervising herdr server pid")
    });
    let adopted = env.running_pid();
    assert_ne!(adopted, plain.id());
    assert_ne!(adopted, anchor.id());
    assert_eq!(wait_exit(&mut plain).code(), Some(0));
    assert!(still_running(&mut anchor), "{}", env.log("anchor.log"));
    assert!(alive(pane_process), "adoption must keep pane processes");
    env.request("pane.list", serde_json::json!({}));

    // launchd stops a job with SIGTERM; the anchor passes it to the server.
    unsafe {
        libc::kill(anchor.id() as libc::pid_t, libc::SIGTERM);
    }
    assert_eq!(
        wait_exit(&mut anchor).code(),
        Some(0),
        "{}",
        env.log("anchor.log")
    );
    assert_eq!(env.record(), format!("stopped {adopted}"));
    assert!(!alive(adopted));
}

#[test]
fn anchor_exits_nonzero_when_the_server_crashes() {
    let _lock = test_lock();
    let env = Env::new();
    let mut anchor = env.spawn(&["server", "--adopt"], "anchor.log");
    wait_for_socket(&env.api_socket, Duration::from_secs(10));
    wait_until("server record", || {
        env.record() == format!("running {}", anchor.id())
    });
    env.handoff();
    let first = wait_record_changes_from(&env, anchor.id());
    // Crash a server that is not the anchor's own child as well.
    env.handoff();
    let second = wait_record_changes_from(&env, first);
    wait_until("anchor to follow", || {
        env.log("anchor.log")
            .contains(&format!("handed off to pid {second}"))
    });

    unsafe {
        libc::kill(second as libc::pid_t, libc::SIGKILL);
    }
    let status = wait_exit(&mut anchor);
    assert_ne!(status.code(), Some(0), "{}", env.log("anchor.log"));
    assert!(status.code().is_some(), "anchor must exit, not be killed");
}

#[test]
fn plain_server_keeps_exiting_after_handoff() {
    let _lock = test_lock();
    let env = Env::new();
    let mut server = env.spawn(&["server"], "server.log");
    wait_for_socket(&env.api_socket, Duration::from_secs(10));
    wait_until("server record", || {
        env.record() == format!("running {}", server.id())
    });
    env.handoff();
    wait_record_changes_from(&env, server.id());
    assert_eq!(wait_exit(&mut server).code(), Some(0));
}
