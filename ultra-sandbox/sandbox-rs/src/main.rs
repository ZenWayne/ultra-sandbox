//! Rust port of the `sandbox` daemon/client.
//!
//! Protocol is byte-compatible with the Go implementation in `../sandbox/main.go`:
//!
//!   frame = | type: u8 | len: u16 BE | payload: [u8; len] |
//!
//! The daemon runs on the host, listens on a unix socket, and executes the
//! commands it receives. The client runs inside the container, speaks the same
//! frame protocol over the socket, and proxies stdin/stdout/stderr/signals.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
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

// server -> client
const FRAME_STDOUT: u8 = 0x11;
const FRAME_STDERR: u8 = 0x12;
const FRAME_EXIT: u8 = 0x13;

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
    sandbox_dir().join("bin")
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
fn setup_daemon_signals(cleanup_path: PathBuf) {
    let mut sigs = Signals::new([SIGINT, SIGTERM]).expect("signal setup");
    thread::spawn(move || {
        if sigs.forever().next().is_some() {
            let _ = fs::remove_file(&cleanup_path);
            process::exit(0);
        }
    });
}

#[cfg(windows)]
fn setup_daemon_signals(cleanup_path: PathBuf) {
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
// Daemon
// ---------------------------------------------------------------------------

fn run_daemon(sock_path: &Path) -> io::Result<()> {
    if let Some(parent) = sock_path.parent() {
        fs::create_dir_all(parent)?;
    }
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

    setup_daemon_signals(sock_path.to_path_buf());

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

fn handle_client(mut conn: UnixStream) -> io::Result<()> {
    let (ftype, data) = match read_frame(&mut conn) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if ftype != FRAME_EXEC {
        return Ok(());
    }

    let req: ExecRequest = match serde_json::from_slice(&data) {
        Ok(req) => req,
        Err(_) => {
            let _ = write_frame(&mut conn, FRAME_STDERR, b"sandbox: invalid exec request\n");
            let _ = write_frame(&mut conn, FRAME_EXIT, &encode_exit(1));
            return Ok(());
        }
    };

    if req.tty {
        handle_pty(conn, req)
    } else {
        handle_pipe(conn, req)
    }
}

fn handle_pipe(conn: UnixStream, req: ExecRequest) -> io::Result<()> {
    let writer: SharedWriter = Arc::new(Mutex::new(conn.try_clone()?));

    let mut builder = Command::new(&req.cmd);
    builder
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

    let mut cmd = CommandBuilder::new(&req.cmd);
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

fn shim_filename_and_content(cmd_name: &str) -> (String, String) {
    #[cfg(unix)]
    {
        (
            cmd_name.to_string(),
            format!("#!/bin/sh\nexec sandbox run {} \"$@\"\n", cmd_name),
        )
    }
    #[cfg(windows)]
    {
        (
            format!("{}.cmd", cmd_name),
            format!("@sandbox run {} %*\r\n", cmd_name),
        )
    }
}

fn run_map(bin_dir: &Path, cmd_name: &str, remove: bool) {
    let (filename, _) = shim_filename_and_content(cmd_name);
    let shim_path = bin_dir.join(&filename);

    if remove {
        if let Err(e) = fs::remove_file(&shim_path) {
            eprintln!("sandbox map: remove {}: {}", shim_path.display(), e);
            process::exit(1);
        }
        println!("removed shim: {}", shim_path.display());
        return;
    }

    // Skip if shim already exists
    if shim_path.exists() {
        println!("mapped (already exists): {} -> sandbox run {}", shim_path.display(), cmd_name);
        return;
    }

    let (_, content) = shim_filename_and_content(cmd_name);
    if let Err(e) = fs::write(&shim_path, content.as_bytes()) {
        eprintln!("sandbox map: write {}: {}", shim_path.display(), e);
        process::exit(1);
    }
    set_permissions(&shim_path, 0o755);
    println!("mapped: {} -> sandbox run {}", shim_path.display(), cmd_name);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  sandbox daemon [--socket PATH]     start host daemon");
    eprintln!("  sandbox run <cmd> [args...]        run command via daemon");
    eprintln!(
        "  sandbox map <cmd> [--remove]       create/remove shim in $SANDBOX_DIR/bin/ \
         (default .ultra_sandbox/bin/)"
    );
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
        "run" => {
            if args.len() < 3 {
                eprintln!("usage: sandbox run <cmd> [args...]");
                process::exit(1);
            }
            run_client(&socket_path(), &args[2], args[3..].to_vec());
        }
        "map" => {
            if args.len() < 3 {
                eprintln!("usage: sandbox map <cmd> [--remove]");
                process::exit(1);
            }
            let cmd_name = &args[2];
            let remove = args.len() > 3 && args[3] == "--remove";
            let bin_dir = shim_bin_dir();
            let _ = fs::create_dir_all(&bin_dir);
            run_map(&bin_dir, cmd_name, remove);
        }
        _ => {
            // Symlink-style invocation: argv[0] is the command name.
            let name = self_name(&args[0]);
            if name != "sandbox" && !name.is_empty() {
                run_client(&socket_path(), &name, args[1..].to_vec());
            }
            usage();
        }
    }
}
