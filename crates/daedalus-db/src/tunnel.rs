//! SSH tunnels to a database host: one `/usr/bin/ssh -N -L` per target, shared by every session
//! that needs it and killed when the last one lets go.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Once, OnceLock, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{OnceCell, oneshot};

use crate::config::{SshTunnel, check_target};

pub const SSH: &str = "/usr/bin/ssh";
const TUNNEL_TIMEOUT: Duration = Duration::from_secs(20);
const ATTEMPTS: u32 = 3;
const FORWARD_FAILED: &str = "Could not request local forwarding.";

/// Keeps a tunnel alive; the tunnel is killed when the last lease is dropped.
pub struct Lease {
    _slot: Arc<Slot>,
    addr: SocketAddr,
}

impl Lease {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    ssh: SshTunnel,
    host: String,
    port: u16,
}

#[derive(Default)]
struct Slot {
    cell: OnceCell<Result<Live, String>>,
}

struct Live {
    process: Process,
    port: u16,
}

type Registry = Mutex<HashMap<Key, Weak<Slot>>>;

static REGISTRY: OnceLock<Registry> = OnceLock::new();
static LIVE: LazyLock<Mutex<HashSet<i32>>> = LazyLock::new(Default::default);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Opens or reuses the tunnel to `host:port` as `ssh` sees it; the error is safe to show.
pub async fn acquire(ssh: &SshTunnel, host: &str, port: u16) -> Result<Lease, String> {
    acquire_with(Path::new(SSH), &[], ssh, host, port, TUNNEL_TIMEOUT).await
}

/// Kills every live tunnel now; for app quit.
pub fn kill_all() {
    let pids: Vec<i32> = lock(&LIVE).drain().collect();
    let dir = pid_dir();
    for pid in pids {
        // SAFETY: a pid stays listed only while its child is unreaped, so the group id is still ours.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        if let Some(dir) = &dir {
            let _ = std::fs::remove_file(dir.join(pid.to_string()));
        }
    }
}

async fn acquire_with(
    program: &Path,
    extra: &[&str],
    ssh: &SshTunnel,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<Lease, String> {
    let key = Key {
        ssh: ssh.clone(),
        host: normalise(host),
        port,
    };
    let slot = {
        let mut registry = lock(REGISTRY.get_or_init(Default::default));
        registry.retain(|_, w| w.strong_count() > 0);
        let usable = registry
            .get(&key)
            .and_then(Weak::upgrade)
            .filter(|s| match s.cell.get() {
                None => true,
                Some(Ok(live)) => live.process.alive(),
                Some(Err(_)) => false,
            });
        usable.unwrap_or_else(|| {
            let slot = Arc::new(Slot::default());
            registry.insert(key.clone(), Arc::downgrade(&slot));
            slot
        })
    };
    let opened = slot
        .cell
        .get_or_init(|| open(program, extra, &key, timeout))
        .await;
    match opened {
        Ok(live) => {
            let addr = (Ipv4Addr::LOCALHOST, live.port).into();
            Ok(Lease { _slot: slot, addr })
        }
        Err(e) => Err(e.clone()),
    }
}

fn normalise(host: &str) -> String {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() { "localhost" } else { host }.to_owned()
}

async fn open(
    program: &Path,
    extra: &[&str],
    key: &Key,
    timeout: Duration,
) -> Result<Live, String> {
    key.ssh.check().map_err(str::to_owned)?;
    check_target(&key.host).map_err(str::to_owned)?;
    sweep_once();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let port = free_port().map_err(|_| "Could not pick a local port for the SSH tunnel")?;
        let (process, ready) = spawn(program, extra, key, port)
            .map_err(|e| format!("Could not start ssh: {}", e.kind()))?;
        match tokio::time::timeout(timeout, ready).await {
            Ok(Ok(Ok(()))) => return Ok(Live { process, port }),
            Ok(Ok(Err(line))) if line == FORWARD_FAILED && attempt < ATTEMPTS => continue,
            Ok(Ok(Err(line))) => return Err(ssh_error(&line, &key.ssh.host)),
            Ok(Err(_)) => return Err("SSH: ssh exited".into()),
            Err(_) => return Err(format!("SSH tunnel timed out after {}s", timeout.as_secs())),
        }
    }
}

fn free_port() -> io::Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

type Ready = oneshot::Receiver<Result<(), String>>;

fn spawn(program: &Path, extra: &[&str], key: &Key, port: u16) -> io::Result<(Process, Ready)> {
    let target = match key.host.contains(':') {
        true => format!("[{}]", key.host),
        false => key.host.clone(),
    };
    let mut cmd = Command::new(program);
    cmd.args([
        "-v",
        "-N",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "RemoteCommand=none",
        "-o",
        "RequestTTY=no",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ConnectTimeout=10",
    ])
    .args(extra)
    .arg("-L")
    .arg(format!("127.0.0.1:{port}:{target}:{}", key.port));
    let ssh = &key.ssh;
    if let Some(p) = ssh.port {
        cmd.arg("-p").arg(p.to_string());
    }
    if !ssh.user.is_empty() {
        cmd.arg("-l").arg(&ssh.user);
    }
    if let Some(identity) = &ssh.identity_file {
        cmd.arg("-i")
            .arg(identity)
            .args(["-o", "IdentitiesOnly=yes"]);
    }
    cmd.arg("--")
        .arg(&ssh.host)
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let stderr = child.stderr.take();
    let pid = child.id() as i32;
    lock(&LIVE).insert(pid);
    write_pidfile(pid);
    let process = Process {
        child: Mutex::new(child),
        pid,
    };
    let (tx, rx) = oneshot::channel();
    match stderr {
        Some(stderr) => watch(stderr, tx)?,
        None => return Err(io::Error::other("no stderr")),
    }
    Ok((process, rx))
}

/// Owns ssh's stderr; the thread never logs or stores debug lines because they hold paths and key fingerprints.
fn watch(stderr: ChildStderr, tx: oneshot::Sender<Result<(), String>>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("db-tunnel".into())
        .spawn(move || {
            let mut tx = Some(tx);
            let mut last: Option<String> = None;
            for line in BufReader::new(stderr).split(b'\n') {
                let Ok(bytes) = line else { break };
                let text = String::from_utf8_lossy(&bytes);
                let line = text.trim();
                if line.starts_with("debug1: Entering interactive session") {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(Ok(()));
                    }
                } else if !line.is_empty() && !line.starts_with("debug") {
                    last = Some(line.to_owned());
                }
            }
            if let Some(tx) = tx.take() {
                let _ = tx.send(Err(last.unwrap_or_else(|| "ssh exited".into())));
            }
        })
        .map(|_| ())
}

struct Process {
    child: Mutex<Child>,
    pid: i32,
}

impl Process {
    fn alive(&self) -> bool {
        // Holding LIVE across the reap keeps kill_all from signalling a pid that was just reaped.
        let mut live = lock(&LIVE);
        let alive = matches!(lock(&self.child).try_wait(), Ok(None));
        if !alive {
            live.remove(&self.pid);
        }
        alive
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        lock(&LIVE).remove(&self.pid);
        let mut child = lock(&self.child);
        // Only an unreaped child still owns its group id; after a reap the pid may belong to someone else.
        if matches!(child.try_wait(), Ok(None)) {
            // SAFETY: the child is unreaped, so the group id cannot have been reused.
            unsafe { libc::killpg(self.pid, libc::SIGKILL) };
            let _ = child.wait();
        }
        remove_pidfile(self.pid);
    }
}

fn ssh_error(line: &str, host: &str) -> String {
    if line.contains("Host key verification failed") {
        format!(
            "SSH: host key not trusted. Run ssh {host} once in a terminal to check and accept it, then connect again."
        )
    } else if line.contains("Permission denied") {
        format!(
            "SSH: {line} Add the key to ssh-agent or set IdentityAgent or IdentityFile in ~/.ssh/config."
        )
    } else {
        format!("SSH: {line}")
    }
}

/// A private directory for pidfiles, or None when it cannot be made safe.
fn pid_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("daedalus-db-tunnels");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    // SAFETY: geteuid has no preconditions.
    let mine = meta.uid() == unsafe { libc::geteuid() };
    (meta.is_dir() && mine && meta.permissions().mode() & 0o077 == 0).then_some(dir)
}

fn write_pidfile(pid: i32) {
    let Some(dir) = pid_dir() else { return };
    let start = process_info(pid).map_or(0, |(_, start, _)| start);
    let _ = std::fs::write(dir.join(pid.to_string()), start.to_string());
}

fn remove_pidfile(pid: i32) {
    if let Some(dir) = pid_dir() {
        let _ = std::fs::remove_file(dir.join(pid.to_string()));
    }
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum Verdict {
    Kill,
    Forget,
    Keep,
}

/// Decides a leftover pidfile from the live process behind its pid: (parent, start time, path).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn stale(info: Option<(u32, u64, PathBuf)>, recorded: u64) -> Verdict {
    match info {
        Some((ppid, start, path)) if start == recorded && path == Path::new(SSH) => match ppid {
            1 => Verdict::Kill,
            _ => Verdict::Keep,
        },
        _ => Verdict::Forget,
    }
}

/// Kills tunnels a crashed Daedalus left behind; pid, start time and path must all still match.
fn sweep_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(sweep);
}

#[cfg(target_os = "macos")]
fn sweep() {
    let Some(dir) = pid_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let recorded = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok())
            .unwrap_or(0);
        match stale(process_info(pid).filter(|_| pid > 1), recorded) {
            Verdict::Keep => continue,
            Verdict::Kill => {
                // SAFETY: the pid was just matched against the recorded start time and path.
                unsafe { libc::killpg(pid, libc::SIGTERM) };
            }
            Verdict::Forget => {}
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

#[cfg(not(target_os = "macos"))]
fn sweep() {}

#[cfg(target_os = "macos")]
fn process_info(pid: i32) -> Option<(u32, u64, PathBuf)> {
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: zeroed is valid for this plain C struct; the kernel fills at most `size` bytes.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as long as the size passed.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(&buf));
    Some((info.pbi_ppid, info.pbi_start_tvsec, path))
}

#[cfg(not(target_os = "macos"))]
fn process_info(_pid: i32) -> Option<(u32, u64, PathBuf)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(name: &str, body: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("daedalus-tunnel-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("ssh");
        let text = format!(
            "#!/bin/sh\n{}",
            body.replace("{dir}", &dir.to_string_lossy())
        );
        std::fs::write(&program, text).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, program)
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn tunnel(host: &str) -> SshTunnel {
        SshTunnel {
            host: host.into(),
            port: None,
            user: String::new(),
            identity_file: None,
        }
    }

    const READY: &str = "echo 'debug1: Entering interactive session.' >&2\nexec sleep 30\n";
    const SECS: Duration = Duration::from_secs(10);

    fn lines(path: &Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |t| t.lines().count())
    }

    fn pid_of(dir: &Path) -> i32 {
        std::fs::read_to_string(dir.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn gone(pid: i32) -> bool {
        for _ in 0..100 {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn ready_after_entering_interactive_session() {
        let (_dir, program) = script("ready", READY);
        let lease = run(acquire_with(
            &program,
            &[],
            &tunnel("t-ready"),
            "db",
            5432,
            SECS,
        ))
        .unwrap();
        assert_eq!(lease.addr().ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(lease.addr().port(), 0);
    }

    #[test]
    fn args_carry_hardening_target_and_double_dash() {
        let (dir, program) = script("args", &format!("echo \"$@\" > {{dir}}/args\n{READY}"));
        let ssh = SshTunnel {
            port: Some(2222),
            user: "me".into(),
            identity_file: Some("/home/x/key".into()),
            ..tunnel("t-args")
        };
        let lease = run(acquire_with(&program, &[], &ssh, "::1", 5432, SECS)).unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        for want in [
            "BatchMode=yes",
            "ExitOnForwardFailure=yes",
            "ControlMaster=no",
            "ControlPath=none",
            "ForwardAgent=no",
        ] {
            assert!(args.contains(want), "{want} missing from {args}");
        }
        assert!(!args.contains("StrictHostKeyChecking"));
        let forward = format!("-L 127.0.0.1:{}:[::1]:5432", lease.addr().port());
        assert!(args.contains(&forward), "{args}");
        assert!(
            args.trim_end()
                .ends_with("-p 2222 -l me -i /home/x/key -o IdentitiesOnly=yes -- t-args"),
            "{args}"
        );
    }

    #[test]
    fn two_acquires_share_one_process() {
        let (dir, program) = script("share", &format!("echo x >> {{dir}}/count\n{READY}"));
        let ssh = tunnel("t-share");
        let (a, b) = run(async {
            futures::join!(
                acquire_with(&program, &[], &ssh, "db", 5432, SECS),
                acquire_with(&program, &[], &ssh, "db", 5432, SECS)
            )
        });
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(lines(&dir.join("count")), 1);
        assert_eq!(a.addr(), b.addr());
    }

    #[test]
    fn last_lease_drop_kills_ssh() {
        let (dir, program) = script("drop", &format!("echo $$ > {{dir}}/pid\n{READY}"));
        let ssh = tunnel("t-drop");
        let (a, b) = run(async {
            let a = acquire_with(&program, &[], &ssh, "db", 5432, SECS)
                .await
                .unwrap();
            let b = acquire_with(&program, &[], &ssh, "db", 5432, SECS)
                .await
                .unwrap();
            (a, b)
        });
        let pid = pid_of(&dir);
        drop(a);
        // SAFETY: signal 0 only probes for existence.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        drop(b);
        assert!(gone(pid));
    }

    #[test]
    fn exit_before_ready_reports_last_line() {
        let (_dir, program) = script(
            "exit",
            "echo 'user@h: Permission denied (publickey).' >&2\nexit 255\n",
        );
        let err = run(acquire_with(
            &program,
            &[],
            &tunnel("t-exit"),
            "db",
            5432,
            SECS,
        ))
        .err()
        .unwrap();
        assert!(err.starts_with("SSH: "), "{err}");
        assert!(err.contains("Permission denied"), "{err}");
    }

    #[test]
    fn forward_failure_retries_with_a_new_port() {
        let body = format!(
            "echo x >> {{dir}}/count\nn=$(wc -l < {{dir}}/count)\nif [ \"$n\" -le 2 ]; then echo 'Could not request local forwarding.' >&2; exit 255; fi\n{READY}"
        );
        let (dir, program) = script("retry", &body);
        let lease = run(acquire_with(
            &program,
            &[],
            &tunnel("t-retry"),
            "db",
            5432,
            SECS,
        ));
        assert!(lease.is_ok(), "{:?}", lease.err());
        assert_eq!(lines(&dir.join("count")), 3);
    }

    #[test]
    fn silent_ssh_times_out_and_is_killed() {
        let (dir, program) = script("silent", "echo $$ > {dir}/pid\nexec sleep 30\n");
        let timeout = Duration::from_millis(300);
        let err = run(acquire_with(
            &program,
            &[],
            &tunnel("t-silent"),
            "db",
            5432,
            timeout,
        ))
        .err()
        .unwrap();
        assert!(err.contains("timed out"), "{err}");
        assert!(gone(pid_of(&dir)));
    }

    #[test]
    fn dead_tunnel_is_replaced() {
        let (dir, program) = script(
            "dead",
            &format!("echo x >> {{dir}}/count\necho $$ > {{dir}}/pid\n{READY}"),
        );
        let ssh = tunnel("t-dead");
        run(async {
            let first = acquire_with(&program, &[], &ssh, "db", 5432, SECS)
                .await
                .unwrap();
            // SAFETY: the pid was written by the live script, still held by `first`.
            unsafe { libc::kill(pid_of(&dir), libc::SIGKILL) };
            let mut held = vec![first];
            for _ in 0..100 {
                if lines(&dir.join("count")) == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                held.push(
                    acquire_with(&program, &[], &ssh, "db", 5432, SECS)
                        .await
                        .unwrap(),
                );
            }
            assert_eq!(lines(&dir.join("count")), 2);
        });
    }

    #[test]
    fn bad_fields_never_spawn() {
        let (dir, program) = script("bad", &format!("echo x >> {{dir}}/count\n{READY}"));
        let err = run(acquire_with(
            &program,
            &[],
            &tunnel("-oProxyCommand=x"),
            "db",
            5432,
            SECS,
        ))
        .err()
        .unwrap();
        assert_eq!(err, "SSH host is not valid");
        let err = run(acquire_with(
            &program,
            &[],
            &tunnel("t-bad"),
            "/var/run/pg.sock",
            5432,
            SECS,
        ))
        .err()
        .unwrap();
        assert!(err.starts_with("Database host"), "{err}");
        assert!(!dir.join("count").exists());
    }

    #[test]
    fn ssh_error_maps_host_key_and_permission_lines() {
        let e = ssh_error("Host key verification failed.", "bastion");
        assert!(e.contains("Run ssh bastion once in a terminal"), "{e}");
        let e = ssh_error("me@h: Permission denied (publickey).", "bastion");
        assert!(e.starts_with("SSH: me@h: Permission denied"), "{e}");
        assert!(e.contains("ssh-agent"), "{e}");
        assert_eq!(
            ssh_error("Connection refused", "h"),
            "SSH: Connection refused"
        );
    }

    #[test]
    fn stale_verdicts() {
        let ssh = || PathBuf::from(SSH);
        assert_eq!(stale(Some((1, 50, ssh())), 50), Verdict::Kill);
        assert_eq!(stale(Some((400, 50, ssh())), 50), Verdict::Keep);
        assert_eq!(stale(Some((1, 51, ssh())), 50), Verdict::Forget);
        assert_eq!(stale(Some((1, 50, "/bin/sh".into())), 50), Verdict::Forget);
        assert_eq!(stale(None, 50), Verdict::Forget);
    }

    #[test]
    #[ignore = "starts a local sshd; set DAEDALUS_TEST_SSHD=1"]
    fn real_ssh_forwards_to_a_local_sshd() {
        use std::io::Read;
        if std::env::var_os("DAEDALUS_TEST_SSHD").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("daedalus-sshd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let keygen = |name: &str| {
            let key = dir.join(name);
            let ok = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&key)
                .status()
                .unwrap()
                .success();
            assert!(ok);
            key
        };
        let host_key = keygen("host");
        let client_key = keygen("client");
        std::fs::copy(dir.join("client.pub"), dir.join("authorized")).unwrap();
        let port = free_port().unwrap();
        let config = format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nStrictModes no\nPidFile {}\n",
            host_key.display(),
            dir.join("authorized").display(),
            dir.join("sshd.pid").display()
        );
        std::fs::write(dir.join("sshd_config"), config).unwrap();
        let mut sshd = Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(dir.join("sshd_config"))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let whoami = Command::new("whoami").output().unwrap().stdout;
        let ssh = SshTunnel {
            host: "127.0.0.1".into(),
            port: Some(port),
            user: String::from_utf8_lossy(&whoami).trim().to_owned(),
            identity_file: Some(client_key),
        };
        let known = format!("UserKnownHostsFile={}", dir.join("kh").display());
        let extra = [
            "-F",
            "/dev/null",
            "-o",
            &known,
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "IdentityAgent=none",
        ];
        let lease = run(acquire_with(
            Path::new(SSH),
            &extra,
            &ssh,
            "127.0.0.1",
            port,
            SECS,
        ));
        let banner = lease
            .as_ref()
            .map(|l| {
                let mut stream = std::net::TcpStream::connect(l.addr()).unwrap();
                let mut buf = [0u8; 8];
                stream.read_exact(&mut buf).unwrap();
                buf
            })
            .map_err(String::clone);
        drop(lease);
        let _ = sshd.kill();
        let _ = sshd.wait();
        assert_eq!(&banner.unwrap(), b"SSH-2.0-");
    }
}
