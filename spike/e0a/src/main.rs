//! Throwaway measurement for U4 PR E, Task E0a. No product code; the lines starting `RESULT` are the evidence.
//!
//! - `afunix <dir>` (Windows): (a) file identity of AF_UNIX socket files, (b) connecting to a stale socket file,
//!   (c) connecting to a listener whose backlog is full. Listener: `uds_windows`, as devvm's QMP client uses.
//! - `afunix-qemu <dir> <qemu-system-x86_64>` (Windows): the same three against QEMU's own QMP socket.
//! - `lock <dir> [qemu-system-x86_64]` (every OS; QEMU on Windows): (d) an exclusive `File::try_lock` (what
//!   `FileLock` uses) on a handle the child inherits, the parent then closing its own; held while the child runs,
//!   released when it exits or is killed. The spawn follows `VmProcess::spawn_frozen` on Windows (contain, detach,
//!   frozen spawn, resume, release) and is a plain detached spawn elsewhere (devvm launches no VM there in 0.1.0).
use std::fs::{File, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio as StdStdio};
use std::time::{Duration, Instant};

use cosca::identity::Resolved;
use cosca::{Fd, Process, Stdio};

/// How long a probe waits for a lock to come free, or a connect to return, before the measurement reports it did not.
const BOUND: Duration = Duration::from_secs(10);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("child") => child(Path::new(&args[2])),
        Some("holder") => holder(Path::new(&args[2]), &args[3], &args[4], &args[6..]),
        Some("lock") => lock(Path::new(&args[2]), args.get(3).map(PathBuf::from)),
        #[cfg(windows)]
        Some("afunix") => afunix::own(Path::new(&args[2])),
        #[cfg(windows)]
        Some("afunix-qemu") => afunix::qemu(Path::new(&args[2]), Path::new(&args[3])),
        other => panic!("unknown mode {other:?}"),
    }
}

/// The generic child: runs until `exit_file` exists.
fn child(exit_file: &Path) {
    while !exit_file.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether another handle to `path` cannot take the exclusive lock.
fn held(path: &Path) -> bool {
    let file = File::options().read(true).write(true).open(path).unwrap();
    match file.try_lock() {
        Ok(()) => {
            file.unlock().unwrap();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(e)) => panic!("probe: {e}"),
    }
}

// (d) the lock --------------------------------------------------------------------------------------------------------

/// The holder is devvm: it locks `lock_path` exclusively, starts `program` (`pass`: with the lock handle as fd 3),
/// closes its own handle, prints `READY <child pid> <own pid>`, then exits (`exit`) or waits for stdin to close
/// (`block`).
fn holder(lock_path: &Path, then: &str, pass: &str, program: &[String]) {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    file.try_lock().expect("holder: lock");
    let mut child_pid = 0;
    if pass == "pass" {
        let mut cmd = cosca::Command::new();
        cmd.arg(&program[0]).args(&program[1..]);
        cmd.fd(Fd::STDIN, Stdio::null()).unwrap();
        cmd.fd(Fd::STDOUT, Stdio::null()).unwrap();
        cmd.fd(Fd::STDERR, Stdio::null()).unwrap();
        cmd.fd(3, Stdio::from_file(file.try_clone().unwrap())).unwrap();
        child_pid = start(cmd);
    }
    drop(file);
    println!("READY {child_pid} {}", std::process::id());
    std::io::stdout().flush().unwrap();
    if then == "block" {
        let _ = std::io::stdin().read(&mut [0u8; 1]);
    }
}

#[cfg(windows)]
fn start(mut cmd: cosca::Command) -> u32 {
    cmd.contain();
    cmd.detached();
    let frozen = cmd.spawn_suspended().unwrap();
    let pid = frozen.id().pid();
    let child = frozen.resume().unwrap();
    child.detach();
    pid
}

#[cfg(not(windows))]
fn start(mut cmd: cosca::Command) -> u32 {
    cmd.kill_on_drop(false);
    cmd.detached();
    let child = cmd.spawn().unwrap();
    let pid = child.id().pid();
    child.detach();
    pid
}

struct Holder {
    child: std::process::Child,
    child_pid: u32,
}

fn start_holder(lock_path: &Path, then: &str, pass: &str, program: &[String]) -> Holder {
    let mut child = StdCommand::new(std::env::current_exe().unwrap())
        .args(["holder", lock_path.to_str().unwrap(), then, pass, "--"])
        .args(program)
        .stdin(StdStdio::piped())
        .stdout(StdStdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(parts[0], "READY", "holder said: {line:?}");
    Holder {
        child_pid: parts[1].parse().unwrap(),
        child,
    }
}

fn process(pid: u32) -> Process {
    match Process::from_pid(pid) {
        Resolved::Found(p) => p,
        other => panic!("resolve {pid}: {other:?}"),
    }
}

/// Probes right after a process is gone: `true` if the lock was free at once. Otherwise waits for it and says how long.
fn free_after_death(label: &str, lock_path: &Path) -> bool {
    let start = Instant::now();
    if !held(lock_path) {
        println!("RESULT {label}: FREE at once");
        return true;
    }
    while held(lock_path) {
        if start.elapsed() > BOUND {
            println!("RESULT {label}: STILL HELD after {BOUND:?}");
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("RESULT {label}: HELD at first, FREE after {:?}", start.elapsed());
    false
}

fn expect_held(label: &str, lock_path: &Path) {
    println!(
        "RESULT {label}: {}",
        if held(lock_path) { "HELD" } else { "FREE (unexpected)" }
    );
}

/// `program` is the child's command line; `ends` makes it exit on its own.
fn suite(label: &str, dir: &Path, program: &[String], ends: &dyn Fn(u32), sock: Option<&Path>) {
    let lock_path = dir.join(format!("{label}.sock.lock"));
    // A killed QEMU leaves its socket file behind; the next one binds a fresh name.
    let start_holder = |lock_path: &Path, then: &str, pass: &str, program: &[String]| {
        if let Some(sock) = sock {
            let _ = std::fs::remove_file(sock);
        }
        start_holder(lock_path, then, pass, program)
    };
    let wait_ready = |pid: u32| {
        if let Some(sock) = sock {
            while std::fs::symlink_metadata(sock).is_err() {
                assert!(
                    matches!(process(pid).is_alive(), cosca::identity::Liveness::Alive),
                    "the child died"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    println!("== {label} control: the holder passes nothing and exits");
    let mut h = start_holder(&lock_path, "exit", "nopass", program);
    h.child.wait().unwrap();
    println!(
        "RESULT {label} control after holder exit: {}",
        if held(&lock_path) { "HELD" } else { "FREE" }
    );

    println!("== {label} S1: the holder passes the lock and exits; the child is then killed");
    let mut h = start_holder(&lock_path, "exit", "pass", program);
    wait_ready(h.child_pid);
    h.child.wait().unwrap();
    expect_held(&format!("{label} S1 holder gone, child running"), &lock_path);
    let p = process(h.child_pid);
    p.kill().unwrap();
    p.wait().unwrap();
    free_after_death(&format!("{label} S1 child killed"), &lock_path);

    println!("== {label} S2: the holder passes the lock and is killed; the child is then killed");
    let mut h = start_holder(&lock_path, "block", "pass", program);
    wait_ready(h.child_pid);
    expect_held(&format!("{label} S2 holder alive, own handle closed"), &lock_path);
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    expect_held(&format!("{label} S2 holder killed, child running"), &lock_path);
    let p = process(h.child_pid);
    p.kill().unwrap();
    p.wait().unwrap();
    free_after_death(&format!("{label} S2 child killed"), &lock_path);

    println!("== {label} S3: the child exits on its own while the holder lives");
    let mut h = start_holder(&lock_path, "block", "pass", program);
    wait_ready(h.child_pid);
    expect_held(&format!("{label} S3 child running"), &lock_path);
    let p = process(h.child_pid);
    ends(h.child_pid);
    p.wait().unwrap();
    free_after_death(&format!("{label} S3 child exited"), &lock_path);
    h.child.kill().unwrap();
    h.child.wait().unwrap();

    println!("== {label} S4: 50 x (holder passes the lock and exits, child killed)");
    let mut unexpected = 0;
    for i in 0..50 {
        let mut h = start_holder(&lock_path, "exit", "pass", program);
        wait_ready(h.child_pid);
        h.child.wait().unwrap();
        let before = held(&lock_path);
        let p = process(h.child_pid);
        p.kill().unwrap();
        p.wait().unwrap();
        let after = held(&lock_path);
        if !before || after {
            unexpected += 1;
            println!("RESULT {label} S4 iteration {i}: held before kill {before}, held right after death {after}");
        }
    }
    println!("RESULT {label} S4: {unexpected} unexpected of 50");
}

fn lock(dir: &Path, qemu: Option<PathBuf>) {
    std::fs::create_dir_all(dir).unwrap();
    let exe = std::env::current_exe().unwrap().to_str().unwrap().to_owned();
    let exit_file = dir.join("generic.exit");
    let _ = std::fs::remove_file(&exit_file);
    let generic = vec![exe, "child".into(), exit_file.to_str().unwrap().to_owned()];
    suite(
        "generic",
        dir,
        &generic,
        &|_| {
            File::create(&exit_file).unwrap();
        },
        None,
    );
    #[cfg(windows)]
    if let Some(qemu) = qemu {
        let sock = dir.join("lockq.sock");
        let program = afunix::qemu_command(&qemu, &sock);
        suite("qemu", dir, &program, &|pid| afunix::quit(&sock, pid), Some(&sock));
    }
    #[cfg(not(windows))]
    assert!(qemu.is_none(), "QEMU runs on Windows only");
}

// (a) (b) (c) AF_UNIX on Windows --------------------------------------------------------------------------------------

#[cfg(windows)]
mod afunix {
    use std::ffi::c_void;
    use std::io::{BufRead, BufReader, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawSocket;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use uds_windows::{UnixListener, UnixStream};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Networking::WinSock::listen;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
    };

    use super::BOUND;

    /// More pending connections than any listener here can hold.
    const CAP: usize = 5000;

    /// `(volume serial, 128-bit file id)` of the file at `path`; with `reparse`, of the file itself, not its target.
    fn file_id(path: &Path, reparse: bool) -> std::io::Result<(u64, u128)> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let flags = FILE_FLAG_BACKUP_SEMANTICS | if reparse { FILE_FLAG_OPEN_REPARSE_POINT } else { 0 };
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: an all-zero FILE_ID_INFO is valid; the handle is open and the size is the struct's.
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast::<c_void>(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        let error = std::io::Error::last_os_error();
        // SAFETY: the handle was opened above.
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return Err(error);
        }
        Ok((info.VolumeSerialNumber, u128::from_le_bytes(info.FileId.Identifier)))
    }

    fn show(id: std::io::Result<(u64, u128)>) -> String {
        match id {
            Ok((volume, file)) => format!("volume {volume:016x} file {file:032x}"),
            Err(e) => format!("ERROR {:?} {e}", e.raw_os_error()),
        }
    }

    fn error(e: &std::io::Error) -> String {
        format!("kind={:?} raw={:?} \"{e}\"", e.kind(), e.raw_os_error())
    }

    fn ids(label: &str, path: &Path) {
        println!(
            "RESULT {label} id, open 1 (reparse flag): {}",
            show(file_id(path, true))
        );
        println!(
            "RESULT {label} id, open 2 (reparse flag): {}",
            show(file_id(path, true))
        );
        println!(
            "RESULT {label} id, no reparse flag:       {}",
            show(file_id(path, false))
        );
        println!(
            "RESULT {label} std metadata (follows): {:?}; symlink_metadata: {:?}",
            std::fs::metadata(path).map(|_| "ok").map_err(|e| e.raw_os_error()),
            std::fs::symlink_metadata(path)
                .map(|_| "ok")
                .map_err(|e| e.raw_os_error()),
        );
    }

    /// Opens connections to `path` without accepting until one fails or does not return.
    fn backlog(label: &str, path: &Path) {
        let mut held = Vec::new();
        let mut slowest = Duration::ZERO;
        for i in 0..CAP {
            let (tx, rx) = mpsc::channel();
            let target = path.to_owned();
            let start = Instant::now();
            std::thread::spawn(move || {
                let _ = tx.send(UnixStream::connect(&target));
            });
            match rx.recv_timeout(BOUND) {
                Ok(Ok(stream)) => {
                    slowest = slowest.max(start.elapsed());
                    held.push(stream);
                }
                Ok(Err(e)) => {
                    println!(
                        "RESULT (c) {label}: connect #{i} failed after {:?} ({} held, slowest ok {slowest:?}): {}",
                        start.elapsed(),
                        held.len(),
                        error(&e)
                    );
                    return;
                }
                Err(_) => {
                    println!(
                        "RESULT (c) {label}: connect #{i} BLOCKED for {BOUND:?} ({} held, slowest ok {slowest:?})",
                        held.len()
                    );
                    return;
                }
            }
        }
        println!("RESULT (c) {label}: {CAP} connects all succeeded (slowest {slowest:?})");
    }

    fn stale(label: &str, path: &Path) {
        println!(
            "RESULT (b) {label}: file exists={} id {}",
            std::fs::symlink_metadata(path).is_ok(),
            show(file_id(path, true))
        );
        match UnixStream::connect(path) {
            Ok(_) => println!("RESULT (b) {label}: connect SUCCEEDED"),
            Err(e) => println!("RESULT (b) {label}: connect failed: {}", error(&e)),
        }
    }

    pub fn own(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let (a, b, c) = (dir.join("a.sock"), dir.join("b.sock"), dir.join("c.sock"));
        for p in [&a, &b, &c] {
            let _ = std::fs::remove_file(p);
        }
        let la = UnixListener::bind(&a).unwrap();
        let lb = UnixListener::bind(&b).unwrap();
        ids("(a) socket a", &a);
        ids("(a) socket b", &b);
        let (ida, idb) = (file_id(&a, true).unwrap(), file_id(&b, true).unwrap());
        println!(
            "RESULT (a) equal across two opens: {}; differs from the second socket: {}",
            ida == file_id(&a, true).unwrap(),
            ida != idb
        );
        let regular = dir.join("regular.lock");
        std::fs::write(&regular, b"").unwrap();
        ids("(a) regular file", &regular);

        // A live listener answers.
        match UnixStream::connect(&b) {
            Ok(_) => println!("RESULT (b) live listener: connect succeeded"),
            Err(e) => println!("RESULT (b) live listener: connect failed: {}", error(&e)),
        }
        drop(la);
        stale("stale socket file (listener dropped, file kept)", &a);
        println!(
            "RESULT (a) id after the listener was dropped: {}",
            show(file_id(&a, true))
        );
        match UnixStream::connect(dir.join("nothing.sock")) {
            Ok(_) => println!("RESULT (b) missing file: connect SUCCEEDED"),
            Err(e) => println!("RESULT (b) missing file: connect failed: {}", error(&e)),
        }
        let before = file_id(&a, true).unwrap();
        std::fs::remove_file(&a).unwrap();
        let again = UnixListener::bind(&a).unwrap();
        println!(
            "RESULT (a) deleted and bound again at the same name: same id = {} ({})",
            before == file_id(&a, true).unwrap(),
            show(file_id(&a, true))
        );
        drop(again);

        backlog("uds_windows listener (backlog 128), nothing accepted", &b);
        drop(lb);

        let lc = UnixListener::bind(&c).unwrap();
        // SAFETY: the socket is the listener's, open.
        let status = unsafe { listen(lc.as_raw_socket() as usize, 1) };
        println!("RESULT (c) listen(backlog 1) on the bound listener returned {status}");
        backlog("same listener after listen(1), nothing accepted", &c);
    }

    pub fn qemu_command(qemu: &Path, sock: &Path) -> Vec<String> {
        let _ = std::fs::remove_file(sock);
        [
            qemu.to_str().unwrap().to_owned(),
            "-machine".into(),
            "none".into(),
            "-nodefaults".into(),
            "-display".into(),
            "none".into(),
            "-chardev".into(),
            format!("socket,id=qmp,path={},server=on,wait=off", sock.display()),
            "-object".into(),
            "monitor-qmp,id=qmp0,chardev=qmp".into(),
        ]
        .into()
    }

    /// Connects as soon as QEMU listens; fails if process `pid` is gone first.
    fn connect_when_listening(sock: &Path, pid: u32) -> UnixStream {
        let alive = || {
            matches!(
                cosca::Process::from_pid(pid),
                cosca::identity::Resolved::Found(p) if matches!(p.is_alive(), cosca::identity::Liveness::Alive)
            )
        };
        loop {
            match UnixStream::connect(sock) {
                Ok(stream) => return stream,
                Err(e) => {
                    assert!(alive(), "QEMU exited before listening: {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// Tells QEMU (process `pid`) to quit over QMP.
    pub fn quit(sock: &Path, pid: u32) {
        let stream = connect_when_listening(sock, pid);
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        (&stream).write_all(b"{\"execute\":\"qmp_capabilities\"}\n").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        (&stream).write_all(b"{\"execute\":\"quit\"}\n").unwrap();
    }

    pub fn qemu(dir: &Path, qemu: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let sock = dir.join("q.sock");
        let program = qemu_command(qemu, &sock);
        let mut child = Command::new(&program[0])
            .args(&program[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        while std::fs::symlink_metadata(&sock).is_err() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "QEMU exited before binding its socket"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(connect_when_listening(&sock, child.id()));
        ids("(a) QEMU socket", &sock);
        let first = file_id(&sock, true).unwrap();
        backlog("QEMU's socket, nothing read from the connections", &sock);
        println!(
            "RESULT (a) QEMU socket id unchanged after the connections: {}",
            first == file_id(&sock, true).unwrap()
        );
        child.kill().unwrap();
        child.wait().unwrap();
        println!(
            "RESULT (a) QEMU socket id after TerminateProcess: {} (same: {})",
            show(file_id(&sock, true)),
            file_id(&sock, true).map(|id| id == first).unwrap_or(false)
        );
        stale("QEMU socket file after the process was killed", &sock);
    }
}
