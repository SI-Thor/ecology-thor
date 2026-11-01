//! protocol.rs — the CLI's side of the wire, and `run`'s command gate.
//!
//! The sequence is thor-admit's, with two citations worth making: the
//! pipe-gated child ("nothing runs before the lease is granted") and
//! the kernel-held lease lock ("a dead client cannot leak a lease") are
//! both *their* design (builder-ide `tools/thor-admit` src/client.rs,
//! src/lease.rs), reused here so the bake-off compares daemons, not
//! client disciplines. The one addition is ours: `--progress-every S`
//! sends `{"op":"progress"}` attestations while the command runs, which
//! is what lets the daemon's stall law tell a working lease from a
//! silent hang.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::sizes;

/// What a command left behind.
pub struct Outcome {
    pub code: i32,
    pub signal: Option<i32>,
    pub peak_rss_bytes: u64,
}

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn connect(path: &Path) -> std::io::Result<Client> {
        let stream = UnixStream::connect(path)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client {
            writer: stream,
            reader,
        })
    }

    fn send(&mut self, value: &serde_json::Value) -> std::io::Result<()> {
        let mut text =
            serde_json::to_string(value).map_err(|e| std::io::Error::other(e.to_string()))?;
        text.push('\n');
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()
    }

    fn read_response(&mut self) -> std::io::Result<serde_json::Value> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the daemon closed the connection",
            ));
        }
        serde_json::from_str(line.trim()).map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn request(&mut self, value: &serde_json::Value) -> std::io::Result<serde_json::Value> {
        self.send(value)?;
        self.read_response()
    }

    /// Acquire a lease: `queued` events stream back while the box is
    /// busy; `granted` is the lease; `rejected` is a refusal with its
    /// reason.
    fn acquire(
        &mut self,
        label: &str,
        peak: &str,
        priority: i32,
        pgid: u32,
        mut on_queued: impl FnMut(usize, usize),
    ) -> std::io::Result<serde_json::Value> {
        let peak_value = if peak == "auto" {
            serde_json::json!("auto")
        } else if let Ok(bytes) = peak.parse::<u64>() {
            serde_json::json!(bytes)
        } else {
            serde_json::json!(peak)
        };
        self.send(&serde_json::json!({
            "op": "acquire", "label": label, "peak": peak_value,
            "priority": priority, "target": {"group": {"pgid": pgid}},
        }))?;
        loop {
            let response = self.read_response()?;
            match response["event"].as_str() {
                Some("queued") => {
                    on_queued(
                        response["position"].as_u64().unwrap_or(0) as usize,
                        response["waiting"].as_u64().unwrap_or(0) as usize,
                    );
                }
                Some("granted") => return Ok(response),
                Some("rejected") => {
                    let reason = response["reason"].as_str().unwrap_or("refused");
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        reason,
                    ));
                }
                Some(other) => {
                    return Err(std::io::Error::other(format!(
                        "unexpected answer from the daemon: {other}"
                    )))
                }
                None => return Err(std::io::Error::other("the daemon answered nonsense")),
            }
        }
    }
}

/// Run a command under a lease (thor-admit's gated-child sequence).
pub fn run_under_lease(
    socket: &Path,
    label: &str,
    peak: &str,
    priority: i32,
    command: &[String],
    progress_every: Option<u64>,
) -> std::io::Result<Outcome> {
    // A pipe the child waits on: the command starts when the lease is real.
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::setpgid(0, 0);
            libc::close(write_fd);
            let mut byte = [0u8; 1];
            let read = libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(read_fd);
            if read == 1 && byte[0] == b'g' {
                exec_command(command);
            }
            libc::_exit(126);
        }
    }
    unsafe {
        libc::setpgid(pid, pid);
        libc::close(read_fd);
    }
    let pgid = pid as u32;

    let asked_at = std::time::Instant::now();
    let mut client = Client::connect(socket)?;
    let grant = match client.acquire(label, peak, priority, pgid, |position, waiting| {
        eprintln!("ecology-admit: {label:?} is {position} of {waiting} in the queue");
    }) {
        Ok(grant) => grant,
        Err(error) => {
            // No lease, so no run: EOF on the pipe is the refusal.
            unsafe {
                libc::close(write_fd);
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
            }
            return Err(error);
        }
    };

    let waited = asked_at.elapsed().as_secs_f64();
    let lease_id = grant["lease"].as_u64().unwrap_or(0);
    let peak_bytes = grant["peak_bytes"].as_u64().unwrap_or(0);
    eprintln!(
        "ecology-admit: {label:?} admitted after {waited:.3}s (peak {}, floor {})",
        sizes::fmt_gib(peak_bytes),
        sizes::fmt_gib(grant["floor_bytes"].as_u64().unwrap_or(0)),
    );

    // The kernel-held lock is what makes the lease real.
    let _lock = lease_lock(grant["lock_path"].as_str().unwrap_or_default())?;
    let _ = client.request(&serde_json::json!({"op": "held", "lease": lease_id}));

    // Now the command may run.
    unsafe {
        libc::write(write_fd, b"g".as_ptr() as *const libc::c_void, 1);
        libc::close(write_fd);
    }

    // The stall law's client half: attest progress while the child runs.
    let progress_thread = progress_every.map(|every| {
        let socket = socket.to_path_buf();
        std::thread::spawn(move || {
            let mut client = match Client::connect(&socket) {
                Ok(c) => c,
                Err(_) => return,
            };
            loop {
                std::thread::sleep(Duration::from_secs(every));
                if client
                    .request(&serde_json::json!({"op": "progress", "lease": lease_id}))
                    .is_err()
                {
                    return;
                }
            }
        })
    });

    let outcome = wait_for(pid)?;
    if let Some(thread) = progress_thread {
        // the attestation thread exits when its next request fails or the
        // daemon stops; it is a daemon of habit, not a leak: the connection
        // closes when this thread's client is dropped at return
        let _ = thread;
    }
    let _ = client.request(&serde_json::json!({
        "op": "release", "lease": lease_id,
        "peak_rss_bytes": outcome.peak_rss_bytes,
    }));
    Ok(outcome)
}

/// The client's lease lock (thor-admit's lease.rs, verbatim in spirit:
/// the descriptor is the lease; the kernel drops it when we die).
fn lease_lock(path: &str) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

fn wait_for(pid: libc::pid_t) -> std::io::Result<Outcome> {
    let mut status: libc::c_int = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let waited = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
    if waited < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let peak_rss_bytes = (usage.ru_maxrss as u64).saturating_mul(1024);
    if libc::WIFEXITED(status) {
        Ok(Outcome {
            code: libc::WEXITSTATUS(status),
            signal: None,
            peak_rss_bytes,
        })
    } else if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        Ok(Outcome {
            code: 128 + signal,
            signal: Some(signal),
            peak_rss_bytes,
        })
    } else {
        Ok(Outcome {
            code: 128,
            signal: None,
            peak_rss_bytes,
        })
    }
}

fn exec_command(command: &[String]) -> ! {
    let program = std::ffi::CString::new(command[0].clone()).unwrap_or_default();
    let argv: Vec<std::ffi::CString> = command
        .iter()
        .map(|part| std::ffi::CString::new(part.clone()).unwrap_or_default())
        .collect();
    let mut pointers: Vec<*const libc::c_char> = argv.iter().map(|part| part.as_ptr()).collect();
    pointers.push(std::ptr::null());
    unsafe { libc::execvp(program.as_ptr(), pointers.as_ptr()) };
    let message = format!(
        "ecology-admit: cannot run {}: {}\n",
        command.first().map(String::as_str).unwrap_or(""),
        std::io::Error::last_os_error()
    );
    unsafe {
        libc::write(2, message.as_ptr() as *const libc::c_void, message.len());
        libc::_exit(127);
    }
}

/// The status read (a one-shot connection).
pub fn status(socket: &Path) -> std::io::Result<String> {
    let mut client = Client::connect(socket)?;
    let answer = client.request(&serde_json::json!({"op": "status"}))?;
    serde_json::to_string_pretty(&answer).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Stop the daemon: pid file + SIGTERM (thor-admit's mechanism).
pub fn stop(socket: &Path) -> std::io::Result<()> {
    let mut client = Client::connect(socket)?;
    let _ = client.request(&serde_json::json!({"op": "ping"}))?;
    let pid_file = socket.with_extension("pid");
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no pid file at {}", pid_file.display()),
            )
        })?;
    let signalled = unsafe { libc::kill(pid, libc::SIGTERM) } == 0;
    if !signalled {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
