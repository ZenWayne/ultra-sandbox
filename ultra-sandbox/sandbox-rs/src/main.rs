//! Rust port of the `sandbox` daemon/client.
//!
//! Protocol is byte-compatible with the Go implementation in `../sandbox/main.go`:
//!
//!   frame = | type: u8 | len: u16 BE | payload: [u8; len] |
//!
//! The daemon runs on the host, listens on a unix socket, and executes the
//! commands it receives. The client runs inside the container, speaks the same
//! frame protocol over the socket, and proxies stdin/stdout/stderr/signals.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::symlink as unix_symlink;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use crossterm::{terminal, tty::IsTty};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use signal_hook::consts::{SIGINT, SIGTERM, SIGWINCH};
#[cfg(unix)]
use signal_hook::iterator::Signals;
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

// ---------------------------------------------------------------------------
// Frame protocol
// ---------------------------------------------------------------------------

// client -> server
const FRAME_EXEC: u8 = 0x01;
const FRAME_STDIN: u8 = 0x02;
const FRAME_RESIZE: u8 = 0x03;
const FRAME_SIGNAL: u8 = 0x04;
const FRAME_EOF: u8 = 0x05;
const FRAME_PING: u8 = 0x06;

// server -> client
const FRAME_STDOUT: u8 = 0x11;
const FRAME_STDERR: u8 = 0x12;
const FRAME_EXIT: u8 = 0x13;
const FRAME_PONG: u8 = 0x14;

const IO_BUF: usize = 32 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct ExecRequest {
    cmd: String,
    args: Vec<String>,
    cwd: String,
    tty: bool,
    rows: u16,
    cols: u16,
}

fn write_frame<W: Write>(w: &mut W, ftype: u8, data: &[u8]) -> io::Result<()> {
    if data.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame payload exceeds u16::MAX",
        ));
    }
    let mut buf = Vec::with_capacity(3 + data.len());
    buf.push(ftype);
    buf.extend_from_slice(&(data.len() as u16).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf)
}

fn read_frame<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 3];
    r.read_exact(&mut hdr)?;
    let ftype = hdr[0];
    let n = u16::from_be_bytes([hdr[1], hdr[2]]) as usize;
    let mut data = vec![0u8; n];
    if n > 0 {
        r.read_exact(&mut data)?;
    }
    Ok((ftype, data))
}

fn encode_exit(code: i32) -> [u8; 4] {
    (code as u32).to_be_bytes()
}

// Shared writer helper: every producer thread locks before writing a frame so
// that frame boundaries cannot interleave on the wire.
type SharedWriter = Arc<Mutex<UnixStream>>;

fn write_frame_locked(w: &SharedWriter, ftype: u8, data: &[u8]) -> io::Result<()> {
    let mut guard = w.lock().expect("shared writer mutex poisoned");
    write_frame(&mut *guard, ftype, data)
}

fn send_spawn_error(w: &SharedWriter, err: &dyn std::fmt::Display) {
    let _ = write_frame_locked(w, FRAME_STDERR, format!("{}\n", err).as_bytes());
    let _ = write_frame_locked(w, FRAME_EXIT, &encode_exit(1));
}

// ---------------------------------------------------------------------------
// Sandbox paths
// ---------------------------------------------------------------------------

fn sandbox_dir() -> PathBuf {
    env::var_os("SANDBOX_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".ultra_sandbox"))
}

fn socket_path() -> PathBuf {
    sandbox_dir().join("daemon.sock")
}

fn shim_bin_dir() -> PathBuf {
    env::var_os("SANDBOX_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| sandbox_dir().join("bin"))
}

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) {
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(windows)]
fn set_permissions(_path: &Path, _mode: u32) {
    // Windows has no POSIX mode bits; skip.
}

#[cfg(unix)]
fn signal_process(pid: u32, sig: u8) {
    unsafe {
        libc::kill(pid as i32, sig as i32);
    }
}

// Find any process listening at `sock_path` (via SO_PEERCRED on a fresh
// connection) and SIGTERM/SIGKILL it. Used by `run_daemon` so that a stale
// daemon — possibly serving a deleted SANDBOX_DIR — does not silently keep
// answering after a fresh launch unlinks-and-rebinds the same path.
#[cfg(target_os = "linux")]
fn evict_listener_at(sock_path: &Path) {
    use std::os::unix::io::AsRawFd;
    use std::time::Duration;

    let stream = match UnixStream::connect(sock_path) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len: libc::socklen_t = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    drop(stream);
    if ret != 0 || cred.pid <= 0 {
        return;
    }
    let pid = cred.pid;
    eprintln!("sandbox daemon: evicting incumbent pid={}", pid);
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(50));
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
    }
    eprintln!("sandbox daemon: pid={} ignored SIGTERM, sending SIGKILL", pid);
    unsafe { libc::kill(pid, libc::SIGKILL) };
    thread::sleep(Duration::from_millis(100));
}

#[cfg(all(unix, not(target_os = "linux")))]
fn evict_listener_at(_sock_path: &Path) {
    // SO_PEERCRED is Linux-specific; on other unices fall back to plain
    // unlink-and-bind (the historical behavior).
}

#[cfg(windows)]
fn evict_listener_at(_sock_path: &Path) {}

#[cfg(unix)]
fn signal_process_group(pgid: u32, sig: u8) {
    unsafe {
        libc::killpg(pgid as i32, sig as i32);
    }
}

#[cfg(windows)]
fn signal_process(pid: u32, _sig: u8) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

#[cfg(windows)]
fn signal_process_group(pid: u32, sig: u8) {
    signal_process(pid, sig);
}

#[cfg(unix)]
fn setup_daemon_signals(cleanup_path: PathBuf, our_inode: Option<u64>) {
    use std::os::unix::fs::MetadataExt;
    let mut sigs = Signals::new([SIGINT, SIGTERM]).expect("signal setup");
    thread::spawn(move || {
        if sigs.forever().next().is_some() {
            // Only unlink if the path still resolves to OUR bound inode.
            // After a `unlink + bind` by a newer daemon, this path points to
            // someone else's socket — removing it would silently kill the
            // live daemon's reachability. Orphaned daemons must leave it alone.
            let safe = match (our_inode, fs::metadata(&cleanup_path)) {
                (Some(ours), Ok(m)) => m.ino() == ours,
                _ => false,
            };
            if safe {
                let _ = fs::remove_file(&cleanup_path);
            }
            process::exit(0);
        }
    });
}

#[cfg(windows)]
fn setup_daemon_signals(cleanup_path: PathBuf, _our_inode: Option<u64>) {
    ctrlc::set_handler(move || {
        let _ = fs::remove_file(&cleanup_path);
        process::exit(0);
    })
    .expect("ctrl-c handler");
}

#[cfg(unix)]
fn setup_client_signals(writer: SharedWriter, is_tty: bool) {
    let sig_list: Vec<i32> = if is_tty {
        vec![SIGINT, SIGTERM, SIGWINCH]
    } else {
        vec![SIGINT, SIGTERM]
    };
    if let Ok(mut signals) = Signals::new(&sig_list) {
        thread::spawn(move || {
            for sig in signals.forever() {
                if sig == SIGWINCH {
                    if let Ok((c, r)) = terminal::size() {
                        let mut b = [0u8; 4];
                        b[0..2].copy_from_slice(&r.to_be_bytes());
                        b[2..4].copy_from_slice(&c.to_be_bytes());
                        let _ = write_frame_locked(&writer, FRAME_RESIZE, &b);
                    }
                } else if sig == SIGINT {
                    let _ = write_frame_locked(&writer, FRAME_SIGNAL, &[SIGINT as u8]);
                } else if sig == SIGTERM {
                    let _ = write_frame_locked(&writer, FRAME_SIGNAL, &[SIGTERM as u8]);
                }
            }
        });
    }
}

#[cfg(windows)]
fn setup_client_signals(writer: SharedWriter, _is_tty: bool) {
    ctrlc::set_handler(move || {
        let _ = write_frame_locked(&writer, FRAME_SIGNAL, &[2]); // 2 = SIGINT
    })
    .ok();
}

// ---------------------------------------------------------------------------
// Command map (whitelist)
// ---------------------------------------------------------------------------

fn command_map_path() -> PathBuf {
    sandbox_dir().join("command-map.json")
}

fn load_command_map() -> HashMap<String, String> {
    let data = fs::read(command_map_path()).unwrap_or_default();
    serde_json::from_slice(&data).unwrap_or_default()
}

fn save_command_map(map: &HashMap<String, String>) {
    let json = serde_json::to_vec_pretty(map).expect("serialize command map");
    if let Err(e) = fs::write(command_map_path(), &json) {
        eprintln!("sandbox: write command-map.json: {}", e);
        process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

fn run_daemon(sock_path: &Path) -> io::Result<()> {
    if let Some(parent) = sock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    evict_listener_at(sock_path);
    let _ = fs::remove_file(sock_path);

    let listener = match UnixListener::bind(sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("sandbox daemon: listen {}: {}", sock_path.display(), e);
            return Err(e);
        }
    };
    set_permissions(sock_path, 0o660);
    eprintln!("sandbox daemon: listening on {}", sock_path.display());

    // Capture the inode we just bound so cleanup can verify ownership before
    // unlinking on signal. Without this, killing an orphan daemon (whose path
    // has been re-bound by a newer one) would unlink the live daemon's file.
    let our_inode = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(sock_path).ok().map(|m| m.ino())
        }
        #[cfg(windows)]
        {
            None
        }
    };
    setup_daemon_signals(sock_path.to_path_buf(), our_inode);

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || {
                    let _ = handle_client(stream);
                });
            }
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

// Probe the daemon at `sock_path`: connect, send PING, expect PONG payload to
// equal our own SANDBOX_DIR. Exits 0 if the incumbent matches, 1 otherwise
// (no socket, no listener, wedged daemon, or daemon serving a stale dir).
fn run_daemon_check(sock_path: &Path) -> ! {
    use std::time::Duration;

    let mut stream = match UnixStream::connect(sock_path) {
        Ok(s) => s,
        Err(_) => process::exit(1),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    if write_frame(&mut stream, FRAME_PING, &[]).is_err() {
        process::exit(1);
    }
    let (ftype, data) = match read_frame(&mut stream) {
        Ok(v) => v,
        Err(_) => process::exit(1),
    };
    if ftype != FRAME_PONG {
        process::exit(1);
    }
    let reported = String::from_utf8_lossy(&data);
    let expected = sandbox_dir().to_string_lossy().into_owned();
    if reported == expected {
        process::exit(0);
    }
    eprintln!(
        "sandbox daemon-check: incumbent SANDBOX_DIR={} expected={}",
        reported, expected
    );
    process::exit(1);
}

fn handle_client(mut conn: UnixStream) -> io::Result<()> {
    let (ftype, data) = match read_frame(&mut conn) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if ftype == FRAME_PING {
        let dir = sandbox_dir().to_string_lossy().into_owned();
        let _ = write_frame(&mut conn, FRAME_PONG, dir.as_bytes());
        return Ok(());
    }
    if ftype != FRAME_EXEC {
        return Ok(());
    }

    let mut req: ExecRequest = match serde_json::from_slice(&data) {
        Ok(req) => req,
        Err(_) => {
            let _ = write_frame(&mut conn, FRAME_STDERR, b"sandbox: invalid exec request\n");
            let _ = write_frame(&mut conn, FRAME_EXIT, &encode_exit(1));
            return Ok(());
        }
    };

    // Whitelist check: only mapped commands are allowed.
    let map = load_command_map();
    match map.get(&req.cmd) {
        Some(resolved) => req.cmd = resolved.clone(),
        None => {
            let msg = format!("sandbox: '{}' is not a mapped command\n", req.cmd);
            let _ = write_frame(&mut conn, FRAME_STDERR, msg.as_bytes());
            let _ = write_frame(&mut conn, FRAME_EXIT, &encode_exit(1));
            return Ok(());
        }
    }

    if req.tty {
        handle_pty(conn, req)
    } else {
        handle_pipe(conn, req)
    }
}

fn handle_pipe(conn: UnixStream, req: ExecRequest) -> io::Result<()> {
    let writer: SharedWriter = Arc::new(Mutex::new(conn.try_clone()?));

    let mut builder = Command::new("/bin/sh");
    builder
        .arg("-c")
        .arg("exec \"$@\"")
        .arg("--")
        .arg(&req.cmd)
        .args(&req.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !req.cwd.is_empty() {
        builder.current_dir(&req.cwd);
    }

    let mut child = match builder.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_spawn_error(&writer, &e);
            return Ok(());
        }
    };

    let child_pid = child.id() as i32;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let writer_out = Arc::clone(&writer);
    let out_handle = thread::spawn(move || {
        let mut buf = [0u8; IO_BUF];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if write_frame_locked(&writer_out, FRAME_STDOUT, &buf[..n]).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let writer_err = Arc::clone(&writer);
    let err_handle = thread::spawn(move || {
        let mut buf = [0u8; IO_BUF];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if write_frame_locked(&writer_err, FRAME_STDERR, &buf[..n]).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    // Read client frames (stdin + signals) on this thread.
    let mut reader = conn;
    loop {
        match read_frame(&mut reader) {
            Err(_) => break,
            Ok((FRAME_EOF, _)) => break,
            Ok((FRAME_STDIN, d)) => {
                let _ = stdin.write_all(&d);
            }
            Ok((FRAME_SIGNAL, d)) if !d.is_empty() && child_pid > 0 => {
                signal_process(child_pid as u32, d[0]);
            }
            _ => {}
        }
    }
    drop(stdin);

    let _ = out_handle.join();
    let _ = err_handle.join();
    let code = match child.wait() {
        Ok(status) => status.code().unwrap_or(-1),
        Err(_) => 1,
    };
    let _ = write_frame_locked(&writer, FRAME_EXIT, &encode_exit(code));
    Ok(())
}

fn handle_pty(conn: UnixStream, req: ExecRequest) -> io::Result<()> {
    let writer: SharedWriter = Arc::new(Mutex::new(conn.try_clone()?));

    let rows = if req.rows == 0 { 24 } else { req.rows };
    let cols = if req.cols == 0 { 80 } else { req.cols };

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            send_spawn_error(&writer, &e);
            return Ok(());
        }
    };

    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg("exec \"$@\"");
    cmd.arg("--");
    cmd.arg(&req.cmd);
    for a in &req.args {
        cmd.arg(a);
    }
    if !req.cwd.is_empty() {
        cmd.cwd(&req.cwd);
    }
    for (k, v) in env::vars_os() {
        cmd.env(k, v);
    }

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            send_spawn_error(&writer, &e);
            return Ok(());
        }
    };
    // Close the slave end in the parent so master reads see EOF once the
    // child terminates.
    drop(pair.slave);

    let pid = child.process_id().unwrap_or(0) as i32;

    let mut master_reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            send_spawn_error(&writer, &e);
            return Ok(());
        }
    };
    let mut master_writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            send_spawn_error(&writer, &e);
            return Ok(());
        }
    };

    let writer_out = Arc::clone(&writer);
    let out_handle = thread::spawn(move || {
        let mut buf = [0u8; IO_BUF];
        loop {
            match master_reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if write_frame_locked(&writer_out, FRAME_STDOUT, &buf[..n]).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    // Main loop: read client frames (stdin + resize + signals).
    let mut reader = conn;
    loop {
        match read_frame(&mut reader) {
            Err(_) => break,
            Ok((FRAME_EOF, _)) => break,
            Ok((FRAME_STDIN, d)) => {
                let _ = master_writer.write_all(&d);
            }
            Ok((FRAME_RESIZE, d)) if d.len() == 4 => {
                let r = u16::from_be_bytes([d[0], d[1]]);
                let c = u16::from_be_bytes([d[2], d[3]]);
                let _ = pair.master.resize(PtySize {
                    rows: r,
                    cols: c,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            Ok((FRAME_SIGNAL, d)) if !d.is_empty() && pid > 0 => {
                signal_process_group(pid as u32, d[0]);
            }
            _ => {}
        }
    }

    let code = match child.wait() {
        Ok(status) => status.exit_code() as i32,
        Err(_) => 1,
    };
    // Drop master fds so the reader thread wakes up with EOF even if the
    // child still had live grandchildren holding the slave.
    drop(master_writer);
    drop(pair.master);
    let _ = out_handle.join();

    let _ = write_frame_locked(&writer, FRAME_EXIT, &encode_exit(code));
    Ok(())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

fn run_client(sock_path: &Path, cmd_name: &str, args: Vec<String>) -> ! {
    let conn = match UnixStream::connect(sock_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "sandbox: cannot connect to daemon at {}: {}",
                sock_path.display(),
                e
            );
            eprintln!("sandbox: start daemon with: sandbox daemon");
            process::exit(1);
        }
    };

    let is_tty = io::stdin().is_tty() && io::stdout().is_tty();

    let (cols, rows) = if is_tty {
        terminal::size().unwrap_or((80, 24))
    } else {
        (80, 24)
    };

    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let req = ExecRequest {
        cmd: cmd_name.to_string(),
        args,
        cwd,
        tty: is_tty,
        rows,
        cols,
    };
    let data = match serde_json::to_vec(&req) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("sandbox: encode request: {}", e);
            process::exit(1);
        }
    };

    let mut conn_for_write = match conn.try_clone() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sandbox: clone socket: {}", e);
            process::exit(1);
        }
    };
    if let Err(e) = write_frame(&mut conn_for_write, FRAME_EXEC, &data) {
        eprintln!("sandbox: write error: {}", e);
        process::exit(1);
    }
    let writer: SharedWriter = Arc::new(Mutex::new(conn_for_write));

    if is_tty {
        let _ = terminal::enable_raw_mode();
    }

    setup_client_signals(Arc::clone(&writer), is_tty);

    // stdin -> STDIN frames.
    let writer_in = Arc::clone(&writer);
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; IO_BUF];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => {
                    let _ = write_frame_locked(&writer_in, FRAME_EOF, &[]);
                    return;
                }
                Ok(n) => {
                    if write_frame_locked(&writer_in, FRAME_STDIN, &buf[..n]).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = write_frame_locked(&writer_in, FRAME_EOF, &[]);
                    return;
                }
            }
        }
    });

    // Receive server frames on this thread.
    let mut reader = conn;
    let mut exit_code = 0i32;
    let stdout = io::stdout();
    let stderr = io::stderr();
    loop {
        match read_frame(&mut reader) {
            Err(_) => break,
            Ok((FRAME_STDOUT, d)) => {
                let mut h = stdout.lock();
                let _ = h.write_all(&d);
                let _ = h.flush();
            }
            Ok((FRAME_STDERR, d)) => {
                let mut h = stderr.lock();
                let _ = h.write_all(&d);
                let _ = h.flush();
            }
            Ok((FRAME_EXIT, d)) if d.len() == 4 => {
                exit_code = i32::from_be_bytes([d[0], d[1], d[2], d[3]]);
                break;
            }
            _ => {}
        }
    }

    if is_tty {
        let _ = terminal::disable_raw_mode();
    }
    process::exit(exit_code);
}

// ---------------------------------------------------------------------------
// Map (shim management)
// ---------------------------------------------------------------------------

fn run_map(bin_dir: &Path, cmd_name: &str, exec_path: Option<&str>, remove: bool) {
    let shim_path = bin_dir.join(cmd_name);
    let mut map = load_command_map();

    if remove {
        map.remove(cmd_name);
        if let Err(e) = fs::remove_file(&shim_path) {
            eprintln!("sandbox map: remove {}: {}", shim_path.display(), e);
            process::exit(1);
        }
        save_command_map(&map);
        println!("removed shim: {}", shim_path.display());
        return;
    }

    let target = exec_path.unwrap_or(cmd_name).to_string();

    // Remove existing shim if present so symlink can be (re)created
    if shim_path.exists() || shim_path.is_symlink() {
        let _ = fs::remove_file(&shim_path);
    }

    // Create symlink: alias -> sandbox binary in the same bin dir
    let sandbox_bin = bin_dir.join("sandbox");
    #[cfg(unix)]
    if let Err(e) = unix_symlink(&sandbox_bin, &shim_path) {
        eprintln!("sandbox map: symlink {}: {}", shim_path.display(), e);
        process::exit(1);
    }
    #[cfg(windows)]
    {
        eprintln!("sandbox map: symlinks not supported on Windows");
        process::exit(1);
    }

    map.insert(cmd_name.to_string(), target.clone());
    save_command_map(&map);
    println!("mapped: {} -> sandbox [resolves to: {}]", shim_path.display(), target);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  sandbox daemon [--socket PATH]              start host daemon");
    eprintln!("  sandbox daemon-check                        probe daemon at $SANDBOX_DIR; exit 0 if healthy and matching");
    eprintln!("  sandbox run <cmd> [args...]                 run whitelisted command via daemon");
    eprintln!(
        "  sandbox map <alias> [--exec PATH] [--remove]  create/remove symlink shim in \
         $SANDBOX_BIN_DIR (default $SANDBOX_DIR/bin)"
    );
    eprintln!("    --exec PATH  resolve alias to a specific host script/binary path");
    process::exit(1);
}

fn self_name(argv0: &str) -> String {
    Path::new(argv0)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Symlink-style invocation must be checked before the arg-count guard,
    // because `podman` (symlink → sandbox) called with no args has len == 1.
    let name = self_name(&args[0]);
    if name != "sandbox" && !name.is_empty() {
        run_client(&socket_path(), &name, args[1..].to_vec());
    }

    if args.len() < 2 {
        usage();
    }

    match args[1].as_str() {
        "daemon" => {
            let mut sock = socket_path();
            let mut i = 2usize;
            while i + 1 < args.len() {
                if args[i] == "--socket" {
                    sock = PathBuf::from(&args[i + 1]);
                }
                i += 1;
            }
            if let Err(e) = run_daemon(&sock) {
                eprintln!("sandbox daemon: {}", e);
                process::exit(1);
            }
        }
        "daemon-check" => {
            run_daemon_check(&socket_path());
        }
        "run" => {
            if args.len() < 3 {
                eprintln!("usage: sandbox run <cmd> [args...]");
                process::exit(1);
            }
            run_client(&socket_path(), &args[2], args[3..].to_vec());
        }
        "map" => {
            if args.len() < 3 {
                eprintln!("usage: sandbox map <alias> [--exec PATH] [--remove]");
                process::exit(1);
            }
            let cmd_name = &args[2];
            let mut exec_path: Option<String> = None;
            let mut remove = false;
            let mut i = 3usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--remove" => remove = true,
                    "--exec" => {
                        i += 1;
                        if i >= args.len() {
                            eprintln!("sandbox map: --exec requires a path argument");
                            process::exit(1);
                        }
                        exec_path = Some(args[i].clone());
                    }
                    _ => {}
                }
                i += 1;
            }
            let bin_dir = shim_bin_dir();
            let _ = fs::create_dir_all(&bin_dir);
            run_map(&bin_dir, cmd_name, exec_path.as_deref(), remove);
        }
        _ => {
            usage();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
