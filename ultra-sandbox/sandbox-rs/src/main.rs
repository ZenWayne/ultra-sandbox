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
// Policy: types and persistence
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct CommandPolicy {
    #[serde(default)]
    deny: Vec<Vec<String>>,
    #[serde(default)]
    allow: Vec<Vec<String>>,
}

type PolicyMap = HashMap<String, CommandPolicy>;

#[derive(Debug)]
enum PolicyDenial {
    Deny(Vec<String>),
    AllowMiss,
}

#[derive(Debug, Clone, Copy)]
enum PolicyListKind {
    Deny,
    Allow,
}

fn policy_path() -> PathBuf {
    sandbox_dir().join("policy.json")
}

fn load_policy_at(path: &Path) -> PolicyMap {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(_) => return PolicyMap::new(),
    };
    let mut map: PolicyMap = match serde_json::from_slice(&data) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "sandbox: warning: {} is not valid JSON ({}); ignoring policy",
                path.display(),
                e
            );
            return PolicyMap::new();
        }
    };
    // Drop empty rules defensively — they would match every path.
    for entry in map.values_mut() {
        entry.deny.retain(|r| !r.is_empty());
        entry.allow.retain(|r| !r.is_empty());
    }
    map
}

fn load_policy() -> PolicyMap {
    load_policy_at(&policy_path())
}

fn save_policy_at(path: &Path, map: &PolicyMap) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(map)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn save_policy(map: &PolicyMap) -> io::Result<()> {
    save_policy_at(&policy_path(), map)
}

// ---------------------------------------------------------------------------
// Policy: matching
// ---------------------------------------------------------------------------

fn extract_path(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            i += 1;
            while i < args.len() {
                out.push(args[i].clone());
                i += 1;
            }
            break;
        }
        if a.starts_with('-') {
            if a.contains('=') {
                i += 1;
            } else {
                i += 2;
            }
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

fn matches_path(rule: &[String], path: &[String]) -> bool {
    rule.len() <= path.len()
        && rule.iter().zip(path.iter()).all(|(r, p)| r == p)
}

fn check_policy(
    policy: Option<&CommandPolicy>,
    path: &[String],
) -> Result<(), PolicyDenial> {
    let Some(p) = policy else {
        return Ok(());
    };

    for rule in &p.deny {
        if matches_path(rule, path) {
            return Err(PolicyDenial::Deny(rule.clone()));
        }
    }
    if !p.allow.is_empty() {
        let ok = p.allow.iter().any(|rule| matches_path(rule, path));
        if !ok {
            return Err(PolicyDenial::AllowMiss);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Policy: mutation helpers (pure, operate on PolicyMap in memory)
// ---------------------------------------------------------------------------

fn add_rule(map: &mut PolicyMap, cmd: &str, rule: Vec<String>, kind: PolicyListKind) {
    let entry = map.entry(cmd.to_string()).or_default();
    let target = match kind {
        PolicyListKind::Deny => &mut entry.deny,
        PolicyListKind::Allow => &mut entry.allow,
    };
    if !target.contains(&rule) {
        target.push(rule);
    }
}

fn remove_rule(map: &mut PolicyMap, cmd: &str, rule: &[String]) -> bool {
    let Some(entry) = map.get_mut(cmd) else {
        return false;
    };
    let before = entry.deny.len() + entry.allow.len();
    entry.deny.retain(|r| r.as_slice() != rule);
    entry.allow.retain(|r| r.as_slice() != rule);
    let after = entry.deny.len() + entry.allow.len();
    before != after
}

fn clear_command_policy(map: &mut PolicyMap, cmd: &str) -> bool {
    map.remove(cmd).is_some()
}

// ---------------------------------------------------------------------------
// Policy: human-readable formatting (for `sandbox policy list`)
// ---------------------------------------------------------------------------

fn format_command_policy(name: &str, policy: Option<&CommandPolicy>) -> String {
    let Some(p) = policy else {
        return format!("no policy for {}\n", name);
    };
    let mut out = String::new();
    out.push_str(name);
    out.push_str(":\n");

    out.push_str("  deny:  ");
    if p.deny.is_empty() {
        out.push_str("(empty)\n");
    } else {
        for (i, r) in p.deny.iter().enumerate() {
            if i > 0 {
                out.push_str("         ");
            }
            out.push_str(&r.join(" "));
            out.push('\n');
        }
    }

    out.push_str("  allow: ");
    if p.allow.is_empty() {
        out.push_str("(empty)\n");
    } else {
        for (i, r) in p.allow.iter().enumerate() {
            if i > 0 {
                out.push_str("         ");
            }
            out.push_str(&r.join(" "));
            out.push('\n');
        }
    }
    out
}

fn format_full_policy(map: &PolicyMap) -> String {
    if map.is_empty() {
        return "no policy configured\n".to_string();
    }
    let mut names: Vec<&String> = map.keys().collect();
    names.sort();
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format_command_policy(name, map.get(*name)));
    }
    out
}

// ---------------------------------------------------------------------------
// Policy: CLI dispatcher
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum PolicyOp {
    Add { cmd: String, path: Vec<String>, kind: PolicyListKind },
    Unset { cmd: String, path: Vec<String> },
    Clear { cmd: String },
    List { cmd: Option<String> },
}

fn parse_policy_args(args: &[String]) -> Result<PolicyOp, String> {
    let verb = args.first().ok_or_else(|| "missing verb".to_string())?;
    match verb.as_str() {
        "deny" | "allow" => {
            let kind = if verb == "deny" {
                PolicyListKind::Deny
            } else {
                PolicyListKind::Allow
            };
            let cmd = args
                .get(1)
                .ok_or_else(|| format!("usage: sandbox policy {} <cmd> <subcmd-path...>", verb))?
                .clone();
            let path: Vec<String> = args.iter().skip(2).cloned().collect();
            if path.is_empty() {
                return Err(format!(
                    "usage: sandbox policy {} <cmd> <subcmd-path...>",
                    verb
                ));
            }
            Ok(PolicyOp::Add { cmd, path, kind })
        }
        "unset" => {
            let cmd = args
                .get(1)
                .ok_or_else(|| "usage: sandbox policy unset <cmd> <subcmd-path...>".to_string())?
                .clone();
            let path: Vec<String> = args.iter().skip(2).cloned().collect();
            if path.is_empty() {
                return Err("usage: sandbox policy unset <cmd> <subcmd-path...>".to_string());
            }
            Ok(PolicyOp::Unset { cmd, path })
        }
        "clear" => {
            let cmd = args
                .get(1)
                .ok_or_else(|| "usage: sandbox policy clear <cmd>".to_string())?
                .clone();
            if args.len() > 2 {
                return Err("usage: sandbox policy clear <cmd>".to_string());
            }
            Ok(PolicyOp::Clear { cmd })
        }
        "list" => {
            let cmd = args.get(1).cloned();
            if args.len() > 2 {
                return Err("usage: sandbox policy list [<cmd>]".to_string());
            }
            Ok(PolicyOp::List { cmd })
        }
        other => Err(format!(
            "sandbox policy: unknown verb '{}' (expected deny|allow|unset|list|clear)",
            other
        )),
    }
}

fn run_policy(args: &[String]) {
    let op = match parse_policy_args(args) {
        Ok(op) => op,
        Err(e) => {
            eprintln!("{}", e);
            process::exit(1);
        }
    };

    let path = policy_path();
    let mut map = load_policy_at(&path);

    match op {
        PolicyOp::Add { cmd, path: rule, kind } => {
            add_rule(&mut map, &cmd, rule, kind);
            if let Err(e) = save_policy_at(&path, &map) {
                eprintln!("sandbox policy: write {}: {}", path.display(), e);
                process::exit(1);
            }
        }
        PolicyOp::Unset { cmd, path: rule } => {
            if !remove_rule(&mut map, &cmd, &rule) {
                eprintln!(
                    "sandbox policy: no such rule for '{}': {}",
                    cmd,
                    rule.join(" ")
                );
                process::exit(1);
            }
            if let Err(e) = save_policy_at(&path, &map) {
                eprintln!("sandbox policy: write {}: {}", path.display(), e);
                process::exit(1);
            }
        }
        PolicyOp::Clear { cmd } => {
            if !clear_command_policy(&mut map, &cmd) {
                eprintln!("sandbox policy: no policy for '{}'", cmd);
                process::exit(1);
            }
            if let Err(e) = save_policy_at(&path, &map) {
                eprintln!("sandbox policy: write {}: {}", path.display(), e);
                process::exit(1);
            }
        }
        PolicyOp::List { cmd: None } => {
            print!("{}", format_full_policy(&map));
        }
        PolicyOp::List { cmd: Some(c) } => {
            print!("{}", format_command_policy(&c, map.get(&c)));
        }
    }
}

// ---------------------------------------------------------------------------
// Policy: denial messages (used by handle_client)
// ---------------------------------------------------------------------------

fn format_denial_message(cmd: &str, denial: &PolicyDenial) -> String {
    match denial {
        PolicyDenial::Deny(rule) => {
            format!("sandbox: '{} {}' denied by policy\n", cmd, rule.join(" "))
        }
        PolicyDenial::AllowMiss => {
            format!("sandbox: '{}' not in allow-list\n", cmd)
        }
    }
}

fn format_denial_message_allow_miss(cmd: &str, attempted_path: &[String]) -> String {
    if attempted_path.is_empty() {
        format!("sandbox: '{}' not in allow-list\n", cmd)
    } else {
        format!(
            "sandbox: '{} {}' not in allow-list\n",
            cmd,
            attempted_path.join(" ")
        )
    }
}

fn format_denial_log(cmd: &str, argv: &[String], denial: &PolicyDenial) -> String {
    let argv_joined = argv.join(" ");
    let argv_part = if argv_joined.is_empty() {
        String::new()
    } else {
        format!(" {}", argv_joined)
    };
    match denial {
        PolicyDenial::Deny(rule) => format!(
            "sandbox daemon: blocked {}{} (deny rule: {})",
            cmd,
            argv_part,
            rule.join(" ")
        ),
        PolicyDenial::AllowMiss => format!(
            "sandbox daemon: blocked {}{} (allow-list miss)",
            cmd, argv_part
        ),
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
    let map_key = req.cmd.clone();
    match map.get(&req.cmd) {
        Some(resolved) => req.cmd = resolved.clone(),
        None => {
            let msg = format!("sandbox: '{}' is not a mapped command\n", req.cmd);
            let _ = write_frame(&mut conn, FRAME_STDERR, msg.as_bytes());
            let _ = write_frame(&mut conn, FRAME_EXIT, &encode_exit(1));
            return Ok(());
        }
    }

    // Policy check: deny-list / allow-list per mapped command.
    let policy_map = load_policy();
    let path = extract_path(&req.args);
    if let Err(denial) = check_policy(policy_map.get(&map_key), &path) {
        let user_msg = match &denial {
            PolicyDenial::Deny(_) => format_denial_message(&map_key, &denial),
            PolicyDenial::AllowMiss => format_denial_message_allow_miss(&map_key, &path),
        };
        let log_line = format_denial_log(&map_key, &req.args, &denial);
        eprintln!("{}", log_line);
        let _ = write_frame(&mut conn, FRAME_STDERR, user_msg.as_bytes());
        let _ = write_frame(&mut conn, FRAME_EXIT, &encode_exit(126));
        return Ok(());
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
    eprintln!("  sandbox policy deny|allow|unset <cmd> <subcmd-path...>  manage per-command policy");
    eprintln!("  sandbox policy list [<cmd>]                              show current policy");
    eprintln!("  sandbox policy clear <cmd>                               drop all rules for cmd");
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
        "policy" => {
            let rest: Vec<String> = args.iter().skip(2).cloned().collect();
            run_policy(&rest);
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

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    fn rule(parts: &[&str]) -> Vec<String> {
        s(parts)
    }

    #[test]
    fn extract_path_empty() {
        assert_eq!(extract_path(&[] as &[String]), Vec::<String>::new());
    }

    #[test]
    fn extract_path_simple_positional() {
        assert_eq!(extract_path(&s(&["rm", "myctr"])), s(&["rm", "myctr"]));
    }

    #[test]
    fn extract_path_skips_flag_with_equals() {
        assert_eq!(
            extract_path(&s(&["--log-level=debug", "rm", "myctr"])),
            s(&["rm", "myctr"])
        );
    }

    #[test]
    fn extract_path_skips_flag_and_value() {
        assert_eq!(
            extract_path(&s(&["--log-level", "debug", "rm"])),
            s(&["rm"])
        );
    }

    #[test]
    fn extract_path_double_dash_ends_options() {
        assert_eq!(extract_path(&s(&["--", "rm"])), s(&["rm"]));
    }

    #[test]
    fn extract_path_short_flag_swallows_next_token() {
        // Documented limitation: -f is treated as taking a value, so "rm" is consumed.
        assert_eq!(extract_path(&s(&["-f", "rm"])), Vec::<String>::new());
    }

    #[test]
    fn extract_path_mixed_leading_equals_flag() {
        assert_eq!(
            extract_path(&s(&["--log-level=debug", "system", "prune"])),
            s(&["system", "prune"])
        );
    }

    #[test]
    fn extract_path_equals_flag_in_middle_preserves_path() {
        assert_eq!(
            extract_path(&s(&["system", "--force=true", "prune"])),
            s(&["system", "prune"])
        );
    }

    #[test]
    fn extract_path_bool_flag_in_middle_swallows_next() {
        // Documented limitation: --force is assumed to take a value, so "prune" is consumed.
        assert_eq!(
            extract_path(&s(&["system", "--force", "prune"])),
            s(&["system"])
        );
    }

    #[test]
    fn matches_path_rule_is_prefix_of_path() {
        assert!(matches_path(&s(&["rm"]), &s(&["rm", "myctr"])));
    }

    #[test]
    fn matches_path_exact_equality() {
        assert!(matches_path(&s(&["system", "prune"]), &s(&["system", "prune"])));
    }

    #[test]
    fn matches_path_rule_longer_than_path() {
        assert!(!matches_path(&s(&["system", "prune"]), &s(&["system"])));
    }

    #[test]
    fn matches_path_token_equality_not_substring() {
        assert!(!matches_path(&s(&["rm"]), &s(&["rmi"])));
    }

    #[test]
    fn matches_path_empty_rule_matches_anything() {
        // Defensive: empty rules are dropped on load (Task 5), but if one slips
        // through it would match every path. Lock that behavior in so a future
        // refactor can't silently change it.
        assert!(matches_path(&[] as &[String], &s(&["anything"])));
        assert!(matches_path(&[] as &[String], &[] as &[String]));
    }

    #[test]
    fn check_policy_none_is_ok() {
        assert!(matches!(check_policy(None, &s(&["rm"])), Ok(())));
    }

    #[test]
    fn check_policy_empty_lists_is_ok() {
        let p = CommandPolicy::default();
        assert!(matches!(check_policy(Some(&p), &s(&["rm"])), Ok(())));
    }

    #[test]
    fn check_policy_matching_deny_returns_deny_with_rule() {
        let p = CommandPolicy {
            deny: vec![rule(&["rm"])],
            allow: vec![],
        };
        match check_policy(Some(&p), &s(&["rm", "foo"])) {
            Err(PolicyDenial::Deny(r)) => assert_eq!(r, rule(&["rm"])),
            other => panic!("expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn check_policy_allow_miss_returns_allow_miss() {
        let p = CommandPolicy {
            deny: vec![],
            allow: vec![rule(&["get"]), rule(&["describe"])],
        };
        assert!(matches!(
            check_policy(Some(&p), &s(&["delete", "pods", "foo"])),
            Err(PolicyDenial::AllowMiss)
        ));
    }

    #[test]
    fn check_policy_allow_hit_is_ok() {
        let p = CommandPolicy {
            deny: vec![],
            allow: vec![rule(&["get"])],
        };
        assert!(matches!(
            check_policy(Some(&p), &s(&["get", "pods"])),
            Ok(())
        ));
    }

    #[test]
    fn check_policy_deny_wins_over_allow_match() {
        // Both deny and allow would match. Deny is checked first and must win.
        let p = CommandPolicy {
            deny: vec![rule(&["system", "prune"])],
            allow: vec![rule(&["system"])],
        };
        match check_policy(Some(&p), &s(&["system", "prune", "-a"])) {
            Err(PolicyDenial::Deny(r)) => assert_eq!(r, rule(&["system", "prune"])),
            other => panic!("expected Deny(system prune), got {:?}", other),
        }
    }

    #[test]
    fn check_policy_empty_path_with_active_allow_is_blocked() {
        let p = CommandPolicy {
            deny: vec![],
            allow: vec![rule(&["get"])],
        };
        assert!(matches!(
            check_policy(Some(&p), &[] as &[String]),
            Err(PolicyDenial::AllowMiss)
        ));
    }

    // --- Task 5: persistence tests ---

    use tempfile::TempDir;

    #[test]
    fn save_then_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("policy.json");

        let mut map = PolicyMap::new();
        map.insert(
            "podman".into(),
            CommandPolicy {
                deny: vec![rule(&["rm"]), rule(&["system", "prune"])],
                allow: vec![],
            },
        );
        map.insert(
            "kubectl".into(),
            CommandPolicy {
                deny: vec![],
                allow: vec![rule(&["get"]), rule(&["describe"])],
            },
        );

        save_policy_at(&path, &map).unwrap();
        let loaded = load_policy_at(&path);

        assert_eq!(loaded.len(), 2);
        let p = loaded.get("podman").unwrap();
        assert_eq!(p.deny, vec![rule(&["rm"]), rule(&["system", "prune"])]);
        assert!(p.allow.is_empty());
        let k = loaded.get("kubectl").unwrap();
        assert!(k.deny.is_empty());
        assert_eq!(k.allow, vec![rule(&["get"]), rule(&["describe"])]);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let loaded = load_policy_at(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_malformed_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("policy.json");
        std::fs::write(&path, b"not valid json {{{").unwrap();
        let loaded = load_policy_at(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_drops_empty_rules_defensively() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("policy.json");
        std::fs::write(
            &path,
            br#"{"podman":{"deny":[[],["rm"]],"allow":[[]]}}"#,
        )
        .unwrap();
        let loaded = load_policy_at(&path);
        let p = loaded.get("podman").unwrap();
        assert_eq!(p.deny, vec![rule(&["rm"])]);
        assert!(p.allow.is_empty());
    }

    #[test]
    fn save_uses_atomic_rename() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("policy.json");
        let tmp_path = path.with_extension("json.tmp");

        let map = PolicyMap::new();
        save_policy_at(&path, &map).unwrap();

        assert!(path.exists());
        assert!(!tmp_path.exists());
    }

    // --- Task 6: mutation helper tests ---

    #[test]
    fn add_rule_inserts_into_deny_creating_entry() {
        let mut map = PolicyMap::new();
        add_rule(&mut map, "podman", rule(&["rm"]), PolicyListKind::Deny);
        let p = map.get("podman").unwrap();
        assert_eq!(p.deny, vec![rule(&["rm"])]);
        assert!(p.allow.is_empty());
    }

    #[test]
    fn add_rule_inserts_into_allow() {
        let mut map = PolicyMap::new();
        add_rule(&mut map, "kubectl", rule(&["get"]), PolicyListKind::Allow);
        let p = map.get("kubectl").unwrap();
        assert_eq!(p.allow, vec![rule(&["get"])]);
        assert!(p.deny.is_empty());
    }

    #[test]
    fn add_rule_is_idempotent() {
        let mut map = PolicyMap::new();
        add_rule(&mut map, "podman", rule(&["rm"]), PolicyListKind::Deny);
        add_rule(&mut map, "podman", rule(&["rm"]), PolicyListKind::Deny);
        add_rule(&mut map, "podman", rule(&["rm"]), PolicyListKind::Deny);
        assert_eq!(map.get("podman").unwrap().deny, vec![rule(&["rm"])]);
    }

    #[test]
    fn remove_rule_returns_true_when_present_in_deny() {
        let mut map = PolicyMap::new();
        add_rule(&mut map, "podman", rule(&["rm"]), PolicyListKind::Deny);
        add_rule(&mut map, "podman", rule(&["kill"]), PolicyListKind::Deny);
        assert!(remove_rule(&mut map, "podman", &rule(&["rm"])));
        assert_eq!(map.get("podman").unwrap().deny, vec![rule(&["kill"])]);
    }

    #[test]
    fn remove_rule_returns_true_when_present_in_allow() {
        let mut map = PolicyMap::new();
        add_rule(&mut map, "kubectl", rule(&["get"]), PolicyListKind::Allow);
        assert!(remove_rule(&mut map, "kubectl", &rule(&["get"])));
        assert!(map.get("kubectl").unwrap().allow.is_empty());
    }

    #[test]
    fn remove_rule_returns_false_for_missing_command() {
        let mut map = PolicyMap::new();
        assert!(!remove_rule(&mut map, "podman", &rule(&["rm"])));
    }

    #[test]
    fn remove_rule_returns_false_for_missing_rule() {
        let mut map = PolicyMap::new();
        add_rule(&mut map, "podman", rule(&["rm"]), PolicyListKind::Deny);
        assert!(!remove_rule(&mut map, "podman", &rule(&["kill"])));
    }

    #[test]
    fn clear_command_policy_removes_entry() {
        let mut map = PolicyMap::new();
        add_rule(&mut map, "podman", rule(&["rm"]), PolicyListKind::Deny);
        assert!(clear_command_policy(&mut map, "podman"));
        assert!(map.get("podman").is_none());
    }

    #[test]
    fn clear_command_policy_returns_false_when_absent() {
        let mut map = PolicyMap::new();
        assert!(!clear_command_policy(&mut map, "podman"));
    }

    // --- Task 7: format helper tests ---

    #[test]
    fn format_command_policy_with_rules() {
        let p = CommandPolicy {
            deny: vec![rule(&["rm"]), rule(&["system", "prune"]), rule(&["volume", "rm"])],
            allow: vec![],
        };
        let out = format_command_policy("podman", Some(&p));
        let expected = "\
podman:
  deny:  rm
         system prune
         volume rm
  allow: (empty)
";
        assert_eq!(out, expected);
    }

    #[test]
    fn format_command_policy_allow_only() {
        let p = CommandPolicy {
            deny: vec![],
            allow: vec![rule(&["get"]), rule(&["describe"])],
        };
        let out = format_command_policy("kubectl", Some(&p));
        let expected = "\
kubectl:
  deny:  (empty)
  allow: get
         describe
";
        assert_eq!(out, expected);
    }

    #[test]
    fn format_command_policy_absent() {
        let out = format_command_policy("missing", None);
        assert_eq!(out, "no policy for missing\n");
    }

    #[test]
    fn format_full_policy_empty() {
        let map = PolicyMap::new();
        assert_eq!(format_full_policy(&map), "no policy configured\n");
    }

    #[test]
    fn format_full_policy_sorted_by_command_name() {
        let mut map = PolicyMap::new();
        add_rule(&mut map, "zsh", rule(&["completion"]), PolicyListKind::Allow);
        add_rule(&mut map, "podman", rule(&["rm"]), PolicyListKind::Deny);
        add_rule(&mut map, "adb", rule(&["shell"]), PolicyListKind::Allow);
        let out = format_full_policy(&map);
        let adb_pos = out.find("adb:").unwrap();
        let podman_pos = out.find("podman:").unwrap();
        let zsh_pos = out.find("zsh:").unwrap();
        assert!(adb_pos < podman_pos);
        assert!(podman_pos < zsh_pos);
    }

    // --- Task 8: CLI dispatcher tests ---

    #[test]
    fn parse_policy_args_deny() {
        let args = s(&["deny", "podman", "system", "prune"]);
        let op = parse_policy_args(&args).unwrap();
        assert!(matches!(
            op,
            PolicyOp::Add { ref cmd, ref path, kind: PolicyListKind::Deny }
                if cmd == "podman" && path == &rule(&["system", "prune"])
        ));
    }

    #[test]
    fn parse_policy_args_allow() {
        let args = s(&["allow", "kubectl", "get"]);
        let op = parse_policy_args(&args).unwrap();
        assert!(matches!(
            op,
            PolicyOp::Add { ref cmd, ref path, kind: PolicyListKind::Allow }
                if cmd == "kubectl" && path == &rule(&["get"])
        ));
    }

    #[test]
    fn parse_policy_args_unset() {
        let args = s(&["unset", "podman", "rm"]);
        let op = parse_policy_args(&args).unwrap();
        assert!(matches!(
            op,
            PolicyOp::Unset { ref cmd, ref path }
                if cmd == "podman" && path == &rule(&["rm"])
        ));
    }

    #[test]
    fn parse_policy_args_clear() {
        let args = s(&["clear", "podman"]);
        let op = parse_policy_args(&args).unwrap();
        assert!(matches!(op, PolicyOp::Clear { ref cmd } if cmd == "podman"));
    }

    #[test]
    fn parse_policy_args_list_all() {
        let args = s(&["list"]);
        let op = parse_policy_args(&args).unwrap();
        assert!(matches!(op, PolicyOp::List { cmd: None }));
    }

    #[test]
    fn parse_policy_args_list_one() {
        let args = s(&["list", "podman"]);
        let op = parse_policy_args(&args).unwrap();
        assert!(matches!(op, PolicyOp::List { cmd: Some(ref c) } if c == "podman"));
    }

    #[test]
    fn parse_policy_args_deny_without_path_errors() {
        let args = s(&["deny", "podman"]);
        assert!(parse_policy_args(&args).is_err());
    }

    #[test]
    fn parse_policy_args_unknown_verb_errors() {
        let args = s(&["bogus", "podman"]);
        assert!(parse_policy_args(&args).is_err());
    }

    #[test]
    fn parse_policy_args_no_verb_errors() {
        let args = s(&[]);
        assert!(parse_policy_args(&args).is_err());
    }

    // --- Task 10: denial message format tests ---

    #[test]
    fn format_denial_message_for_deny_rule() {
        let denial = PolicyDenial::Deny(rule(&["system", "prune"]));
        let msg = format_denial_message("podman", &denial);
        assert_eq!(msg, "sandbox: 'podman system prune' denied by policy\n");
    }

    #[test]
    fn format_denial_message_for_allow_miss() {
        let denial = PolicyDenial::AllowMiss;
        let path = rule(&["exec", "myctr", "bash"]);
        let msg = format_denial_message_allow_miss("podman", &path);
        assert_eq!(msg, "sandbox: 'podman exec myctr bash' not in allow-list\n");
    }

    #[test]
    fn format_denial_log_line_deny() {
        let argv = s(&["rm", "myctr"]);
        let denial = PolicyDenial::Deny(rule(&["rm"]));
        let line = format_denial_log("podman", &argv, &denial);
        assert_eq!(line, "sandbox daemon: blocked podman rm myctr (deny rule: rm)");
    }

    #[test]
    fn format_denial_log_line_allow_miss() {
        let argv = s(&["exec", "myctr", "bash"]);
        let denial = PolicyDenial::AllowMiss;
        let line = format_denial_log("podman", &argv, &denial);
        assert_eq!(line, "sandbox daemon: blocked podman exec myctr bash (allow-list miss)");
    }
}
