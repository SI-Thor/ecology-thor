//! daemon.rs — the daemon: leases, queue, watchdog, stall law, journal.

use std::collections::VecDeque;
use std::fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ecology_core::ids::EntityId;
use ecology_core::journal::Reason;
use ecology_core::why::{SpawnState, WhyLedger, WhyState};

use crate::history::{self, PeakRecord};
use crate::journal::DecisionJournal;
use crate::meminfo;
use crate::sizes;

#[derive(Clone, Debug)]
pub struct Config {
    pub socket: PathBuf,
    pub state_dir: PathBuf,
    pub floor: u64,
    pub emergency_floor: u64,
    pub sample_interval: Duration,
    pub stop_grace: u32,
    pub min_victim_rss: u64,
    pub stall_horizon: Option<Duration>,
    pub idle_exit: Option<Duration>,
}

impl Config {
    pub fn defaults(socket: PathBuf, state_dir: PathBuf) -> Self {
        Config {
            socket,
            state_dir,
            floor: 25 << 30,
            emergency_floor: 8 << 30,
            sample_interval: Duration::from_millis(250),
            stop_grace: 8,
            min_victim_rss: 2 << 30,
            stall_horizon: None,
            idle_exit: Some(Duration::from_secs(900)),
        }
    }
}

/// One live lease. The reservation, the target, the WHY purpose.
struct Lease {
    id: u64,
    label: String,
    peak_bytes: u64,
    priority: i32,
    pgid: u32,
    lock_path: PathBuf,
    granted_at: Instant,
    last_progress: Instant,
    stopped_at: Option<Instant>,
    purpose: EntityId,
    rss_now: u64,
    /// `held` has arrived: the client took the flock, and the sampler may
    /// probe it. Before that, the grant→held window is not a leak — a
    /// client that crashed before holding is reaped by the grant grace.
    held: bool,
}

/// One waiter: the acquire request plus the stream to answer.
struct Waiter {
    label: String,
    peak_bytes: u64,
    priority: i32,
    pgid: u32,
    arrived: Instant,
    reply: UnixStream,
}

struct Inner {
    leases: Vec<Lease>,
    queue: VecDeque<Waiter>,
    next_id: u64,
    journal: DecisionJournal,
    history: Vec<PeakRecord>,
    floor: u64,
    emergency_floor: u64,
    stop_grace: u32,
    min_victim_rss: u64,
    stall_horizon: Option<Duration>,
    lease_dir: PathBuf,
    peaks_path: PathBuf,
    /// daemon ticks: whole seconds since the daemon started
    started: Instant,
}

impl Inner {
    fn tick(&self) -> u32 {
        self.started.elapsed().as_secs() as u32
    }

    /// The reservation arithmetic (thor-admit §2.1):
    /// projected = available - Σ(reserved_i - current_i);
    /// admit ⟺ projected - peak >= floor.
    fn projected_fit(&self, peak: u64) -> bool {
        let available = meminfo::mem_available();
        let owed: u64 = self
            .leases
            .iter()
            .map(|l| l.peak_bytes.saturating_sub(l.rss_now))
            .sum();
        available.saturating_sub(owed).saturating_sub(peak) >= self.floor
    }
}

pub struct Daemon {
    config: Config,
    state: Arc<Mutex<Inner>>,
    term: &'static AtomicBool,
}

impl Daemon {
    pub fn new(config: Config, term: &'static AtomicBool) -> std::io::Result<Self> {
        fs::create_dir_all(&config.state_dir)?;
        let journal = DecisionJournal::open(&config.state_dir.join("journal.jsonl"), 4096)?;
        let history = history::load(&config.state_dir.join("peaks.jsonl"));
        let floor = config.floor;
        let emergency_floor = config.emergency_floor;
        let stop_grace = config.stop_grace;
        let min_victim_rss = config.min_victim_rss;
        let stall_horizon = config.stall_horizon;
        let lease_dir = config.state_dir.join("leases");
        fs::create_dir_all(&lease_dir)?;
        let state = Arc::new(Mutex::new(Inner {
            leases: Vec::new(),
            queue: VecDeque::new(),
            next_id: 0,
            journal,
            history,
            floor,
            emergency_floor,
            stop_grace,
            min_victim_rss,
            stall_horizon,
            lease_dir,
            peaks_path: config.state_dir.join("peaks.jsonl"),
            started: Instant::now(),
        }));
        Ok(Daemon {
            config,
            state,
            term,
        })
    }

    pub fn run(self) -> std::io::Result<()> {
        let socket = self.config.socket.clone();
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600));
        }

        // the sampler: watchdog + stall law + lease health, every interval
        {
            let state = Arc::clone(&self.state);
            let interval = self.config.sample_interval;
            let term = self.term;
            std::thread::spawn(move || {
                while !term.load(Ordering::SeqCst) {
                    std::thread::sleep(interval);
                    let mut state = state.lock().unwrap();
                    sample_and_act(&mut state);
                }
            });
        }

        let idle_exit = self.config.idle_exit;
        let mut last_busy = Instant::now();
        let state = Arc::clone(&self.state);

        // the accept loop: one thread per client connection
        while !self.term.load(Ordering::SeqCst) {
            if listener.set_nonblocking(true).is_ok() {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let per_connection = Arc::clone(&state);
                        std::thread::spawn(move || {
                            let _ = serve_connection(stream, per_connection);
                        });
                        let busy = {
                            let state = state.lock().unwrap();
                            !state.leases.is_empty() || !state.queue.is_empty()
                        };
                        if busy {
                            last_busy = Instant::now();
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if let Some(limit) = idle_exit {
                            if !{
                                let state = state.lock().unwrap();
                                !state.leases.is_empty() || !state.queue.is_empty()
                            } && last_busy.elapsed() > limit
                            {
                                let mut state = state.lock().unwrap();
                                state
                                    .journal
                                    .record(serde_json::json!({"event": "daemon.idle_exit"}));
                                return Ok(());
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => return Err(e),
                }
            } else {
                return Err(std::io::Error::last_os_error());
            }
        }

        let mut state = self.state.lock().unwrap();
        state
            .journal
            .record(serde_json::json!({"event": "daemon.stopped"}));
        Ok(())
    }
}

/// One client connection: a loop of JSON lines.
fn serve_connection(stream: UnixStream, state: Arc<Mutex<Inner>>) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            let _ = writeln!(writer, r#"{{"event":"error","message":"not JSON"}}"#);
            continue;
        };
        match request["op"].as_str() {
            Some("ping") => {
                let _ = writeln!(writer, r#"{{"event":"ok"}}"#);
            }
            Some("status") => {
                let answer = {
                    let mut state = state.lock().unwrap();
                    status_locked(&mut state)
                };
                let _ = writeln!(writer, "{answer}");
            }
            Some("acquire") => {
                let label = request["label"].as_str().unwrap_or("unnamed").to_string();
                let priority = request["priority"].as_i64().unwrap_or(0) as i32;
                let pgid = request["target"]["group"]["pgid"].as_u64().unwrap_or(0) as u32;
                let peak = match peak_arg(&request) {
                    Ok(p) => p,
                    Err(reason) => {
                        let _ = writeln!(
                            writer,
                            "{}",
                            serde_json::json!({"event": "rejected", "reason": reason})
                        );
                        continue;
                    }
                };
                // The acquire reply (queued events, then the grant) goes
                // to this connection; a clone rides with the waiter in
                // case the grant comes later, from the sampler. The
                // connection stays open: held / progress / release all
                // arrive on it, served by this same loop.
                let reply = writer.try_clone()?;
                let state_for_acquire = Arc::clone(&state);
                acquire(state_for_acquire, reply, label, peak, priority, pgid)?;
            }
            Some("held") => {
                let id = request["lease"].as_u64().unwrap_or(0);
                let mut state = state.lock().unwrap();
                let held = state.leases.iter_mut().find(|l| l.id == id).map(|lease| {
                    lease.granted_at = Instant::now();
                    (lease.purpose, lease.label.clone())
                });
                if let Some((purpose, label)) = held {
                    let tick = state.tick();
                    state.journal.ledger.run_under(purpose, tick);
                    if let Some(lease) = state.leases.iter_mut().find(|l| l.id == id) {
                        lease.held = true;
                    }
                    state.journal.record(serde_json::json!({
                        "event": "lease.held", "lease": id, "label": label,
                    }));
                }
                let _ = writeln!(writer, r#"{{"event":"ok"}}"#);
            }
            Some("progress") => {
                let id = request["lease"].as_u64().unwrap_or(0);
                let mut state = state.lock().unwrap();
                let touched = state.leases.iter_mut().find(|l| l.id == id).map(|lease| {
                    lease.last_progress = Instant::now();
                    lease.purpose
                });
                if let Some(purpose) = touched {
                    let tick = state.tick();
                    state.journal.ledger.run_under(purpose, tick);
                }
                let _ = writeln!(writer, r#"{{"event":"ok"}}"#);
            }
            Some("release") => {
                let id = request["lease"].as_u64().unwrap_or(0);
                let peak_rss = request["peak_rss_bytes"].as_u64().unwrap_or(0);
                let mut state = state.lock().unwrap();
                release(&mut state, id, peak_rss, "lease.released");
                let _ = writeln!(writer, r#"{{"event":"ok"}}"#);
            }
            _ => {
                let _ = writeln!(
                    writer,
                    "{}",
                    serde_json::json!({"event": "error", "message": "unknown op"})
                );
            }
        }
    }
    Ok(())
}

/// The peak argument: explicit bytes / a size string / "auto".
fn peak_arg(request: &serde_json::Value) -> Result<PeakArg, String> {
    let value = &request["peak"];
    if value.is_null() {
        return Err("no peak given".into());
    }
    if let Some(text) = value.as_str() {
        if text == "auto" {
            return Ok(PeakArg::Auto);
        }
        return sizes::parse_size(text)
            .map(PeakArg::Bytes)
            .ok_or_else(|| "peak is not a size".into());
    }
    value
        .as_u64()
        .filter(|v| *v > 0)
        .map(PeakArg::Bytes)
        .ok_or_else(|| "peak is not a byte count".into())
}

/// A peak as asked: an explicit size, or the learned p95 of the label.
enum PeakArg {
    Auto,
    Bytes(u64),
}

/// `acquire`: resolve the peak (the history for "auto"), check
/// admission, grant or queue (head-of-line, by priority).
fn acquire(
    state: Arc<Mutex<Inner>>,
    mut writer: UnixStream,
    label: String,
    peak: PeakArg,
    priority: i32,
    pgid: u32,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut state = state.lock().unwrap();

    // "auto" is the learned p95 of this label's history — and a label
    // nobody has measured is REFUSED, never guessed (thor-admit's law,
    // ours too: a number that does not say where it came from is a
    // number somebody typed).
    let peak = match peak {
        PeakArg::Bytes(bytes) => bytes,
        PeakArg::Auto => match history::p95_peak(&state.history, &label) {
            Some(bytes) => bytes,
            None => {
                let _ = writeln!(
                    writer,
                    "{}",
                    serde_json::json!({
                        "event": "rejected",
                        "reason": "no history for the label",
                    })
                );
                return Ok(());
            }
        },
    };

    if !state.projected_fit(peak) {
        // queue: the reply stream stays with the waiter; the sampler
        // grants it when the head fits
        let position = state.queue.len() + 1;
        let _ = writeln!(
            writer,
            "{}",
            serde_json::json!({
                "event": "queued", "position": position,
                "waiting": position, "label": label, "peak_bytes": peak,
            })
        );
        state.journal.record(serde_json::json!({
            "event": "lease.queued", "label": label, "peak_bytes": peak,
            "priority": priority,
        }));
        state.queue.push_back(Waiter {
            label,
            peak_bytes: peak,
            priority,
            pgid,
            arrived: Instant::now(),
            reply: writer,
        });
        return Ok(());
    }

    grant(&mut state, label, peak, priority, pgid, &mut writer);
    Ok(())
}

/// Grant a lease: the purpose is born, the client gets the lock path.
fn grant(
    state: &mut Inner,
    label: String,
    peak: u64,
    priority: i32,
    pgid: u32,
    writer: &mut UnixStream,
) {
    use std::io::Write;
    state.next_id += 1;
    let id = state.next_id;
    let lock_path = state.lease_dir.join(format!("lease-{id}.lock"));
    let purpose = state
        .journal
        .ledger
        .spawn(
            None,
            1.0,
            None,
            SpawnState::Alive,
            state.tick(),
            Reason::Born,
        )
        .unwrap_or(EntityId(0));
    state.journal.ledger.run_under(purpose, state.tick());
    state.leases.push(Lease {
        id,
        label: label.clone(),
        peak_bytes: peak,
        priority,
        pgid,
        lock_path: lock_path.clone(),
        granted_at: Instant::now(),
        last_progress: Instant::now(),
        stopped_at: None,
        purpose,
        rss_now: 0,
        held: false,
    });
    state.journal.record(serde_json::json!({
        "event": "lease.granted", "lease": id, "label": label,
        "peak_bytes": peak, "floor_bytes": state.floor,
        "mem_available_bytes": meminfo::mem_available(),
        "priority": priority,
    }));
    let _ = writeln!(
        writer,
        "{}",
        serde_json::json!({
            "event": "granted", "lease": id, "peak_bytes": peak,
            "lock_path": lock_path, "floor_bytes": state.floor,
        })
    );
}

/// Release a lease, learn its peak, admit the queue's head.
fn release(state: &mut Inner, id: u64, peak_rss: u64, event: &str) {
    let Some(position) = state.leases.iter().position(|l| l.id == id) else {
        return;
    };
    let lease = state.leases.remove(position);
    let peak = if peak_rss > 0 {
        peak_rss
    } else {
        lease.rss_now
    };
    let duration = lease.granted_at.elapsed().as_secs_f64();
    let purpose = lease.purpose;
    state.journal.ledger.run_under(purpose, state.tick());
    let label = lease.label.clone();
    state.journal.record(serde_json::json!({
        "event": event, "lease": id, "label": label,
        "peak_rss_bytes": peak, "duration_s": duration,
        "why_state": format!("{:?}", state.journal.ledger.state(purpose)),
    }));
    // learn the peak (the cost model's memory column reads this file).
    // A peak of zero is "we never measured it" (the client vanished
    // before rusage); it teaches nothing and is not written.
    if peak > 0 {
        let record = PeakRecord {
            at: iso_now(),
            label: label.clone(),
            peak_rss_bytes: peak,
            duration_s: duration,
            mem_total_bytes: meminfo::mem_total(),
            floor_bytes: state.floor,
            basis: "measured",
            source: "ecology-admit",
        };
        let _ = history::append(&state.peaks_path, &record);
        state.history.push(record);
    }
    let _ = std::fs::remove_file(&lease.lock_path);
    admit_queue(state);
}

/// Grant the queue's head while it fits (head-of-line blocking: the
/// head must fit before anything behind it is considered — thor-admit
/// §2.2, ours too).
fn admit_queue(state: &mut Inner) {
    loop {
        let Some(head) = state.queue.front() else {
            return;
        };
        let peak = head.peak_bytes;
        if !state.projected_fit(peak) {
            return;
        }
        let waiter = state.queue.pop_front().unwrap();
        let mut reply = waiter.reply;
        grant(
            state,
            waiter.label,
            peak,
            waiter.priority,
            waiter.pgid,
            &mut reply,
        );
    }
}

/// The sampler's pass: refresh, reap the gone, watchdog, stall law,
/// grace kills, then the queue.
fn sample_and_act(state: &mut Inner) {
    let rss = meminfo::rss_by_pgid();
    for lease in state.leases.iter_mut() {
        lease.rss_now = if lease.pgid != 0 {
            rss.get(&lease.pgid).copied().unwrap_or(0)
        } else {
            0
        };
    }

    // A lease whose client is gone (the flock is free) is released by
    // the kernel's own word — and its command is stopped: an unwatched
    // heavy command is exactly what this daemon exists to prevent.
    //
    // The probe applies to leases that have SAID held (the client took
    // the lock). A lease still inside its grant→held window gets a
    // 10-second grace instead — the window is not a leak, and a client
    // that crashed before holding is reaped when the grace expires.
    const HELD_GRACE: Duration = Duration::from_secs(10);
    let gone: Vec<u64> = state
        .leases
        .iter()
        .filter(|l| {
            if l.held {
                !flock_is_held(&l.lock_path)
            } else {
                l.granted_at.elapsed() > HELD_GRACE
            }
        })
        .map(|l| l.id)
        .collect();
    for id in gone {
        let pgid = state
            .leases
            .iter()
            .find(|l| l.id == id)
            .map(|l| l.pgid)
            .unwrap_or(0);
        // the client cannot speak; the kernel's lock can. An unwatched
        // heavy command is the thing this daemon exists to prevent.
        signal_pgid(pgid, libc::SIGTERM);
        signal_pgid(pgid, libc::SIGCONT);
        release(state, id, 0, "lease.client_gone");
    }

    // the watchdog: below the emergency floor, stop the largest lease
    let available = meminfo::mem_available();
    if available < state.emergency_floor {
        let victim = state
            .leases
            .iter()
            .filter(|l| l.pgid != 0 && l.stopped_at.is_none() && l.rss_now >= state.min_victim_rss)
            .max_by_key(|l| l.rss_now)
            .map(|l| (l.id, l.pgid, l.rss_now));
        match victim {
            Some((id, pgid, rss)) => {
                if signal_pgid(pgid, libc::SIGSTOP) {
                    if let Some(lease) = state.leases.iter_mut().find(|l| l.id == id) {
                        lease.stopped_at = Some(Instant::now());
                    }
                    let purpose = state
                        .leases
                        .iter()
                        .find(|l| l.id == id)
                        .map(|l| l.purpose)
                        .unwrap_or(EntityId(0));
                    // the WHY transition: harvested to UNKNOWN —
                    // questionable, not gone
                    state
                        .journal
                        .ledger
                        .doubt(purpose, Reason::Custom, state.tick());
                    let label = state
                        .leases
                        .iter()
                        .find(|l| l.id == id)
                        .map(|l| l.label.clone())
                        .unwrap_or_default();
                    state.journal.record(serde_json::json!({
                        "event": "watchdog.stopped", "lease": id, "label": label,
                        "pgid": pgid, "rss_bytes": rss,
                        "mem_available_bytes": available,
                        "why": "harvested to UNKNOWN (questionable, not gone)",
                    }));
                }
            }
            None => {
                state.journal.record(serde_json::json!({
                    "event": "watchdog.pressure_outside_leases",
                    "mem_available_bytes": available,
                }));
            }
        }
    }

    // the stall law (the ecology's own): no progress attestation for
    // longer than the horizon, with memory fine — the silent hang the
    // byte-based watchdog cannot see.
    if let Some(horizon) = state.stall_horizon {
        let stalled: Vec<(u64, u32, u64)> = state
            .leases
            .iter()
            .filter(|l| l.stopped_at.is_none() && l.last_progress.elapsed() > horizon)
            .map(|l| (l.id, l.pgid, l.rss_now))
            .collect();
        for (id, pgid, rss) in stalled {
            if signal_pgid(pgid, libc::SIGSTOP) {
                if let Some(lease) = state.leases.iter_mut().find(|l| l.id == id) {
                    lease.stopped_at = Some(Instant::now());
                    let purpose = lease.purpose;
                    let label = lease.label.clone();
                    state
                        .journal
                        .ledger
                        .doubt(purpose, Reason::Lapsed, state.tick());
                    state.journal.record(serde_json::json!({
                        "event": "stall.harvested", "lease": id, "label": label,
                        "rss_bytes": rss, "horizon_s": horizon.as_secs_f64(),
                        "why": "no progress attestation; harvested to UNKNOWN",
                    }));
                }
            }
        }
    }

    // the grace: a stopped lease that did not die is killed
    let grace_ms = 250 * state.stop_grace as u64;
    let expired: Vec<(u64, u32, EntityId)> = state
        .leases
        .iter()
        .filter(|l| {
            l.stopped_at
                .map(|t| t.elapsed() > Duration::from_millis(grace_ms))
                .unwrap_or(false)
        })
        .map(|l| (l.id, l.pgid, l.purpose))
        .collect();
    for (id, pgid, purpose) in expired {
        signal_pgid(pgid, libc::SIGTERM);
        signal_pgid(pgid, libc::SIGCONT);
        if let Some(lease) = state.leases.iter_mut().find(|l| l.id == id) {
            lease.stopped_at = None;
        }
        state
            .journal
            .ledger
            .kill(purpose, Reason::Killed, state.tick(), 64);
        let label = state
            .leases
            .iter()
            .find(|l| l.id == id)
            .map(|l| l.label.clone())
            .unwrap_or_default();
        state.journal.record(serde_json::json!({
            "event": "watchdog.killed", "lease": id, "label": label,
            "waited_ms": grace_ms,
            "why": "dead, reason recorded (the grace period ran out)",
        }));
    }

    admit_queue(state);
}

fn signal_pgid(pgid: u32, signal: i32) -> bool {
    if pgid == 0 {
        return false;
    }
    unsafe { libc::kill(-(pgid as i32), signal) == 0 }
}

/// Is anybody holding the flock? The daemon answers by *trying* to take
/// it (thor-admit §3): if it can, nobody was.
fn flock_is_held(path: &std::path::Path) -> bool {
    let Ok(file) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    else {
        return true; // unreadable is held (conservative)
    };
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        false
    } else {
        true
    }
}

fn status_locked(state: &mut Inner) -> String {
    let leases: Vec<serde_json::Value> = state
        .leases
        .iter()
        .map(|l| {
            serde_json::json!({
                "lease": l.id, "label": l.label, "pgid": l.pgid,
                "peak_bytes": l.peak_bytes, "rss_bytes": l.rss_now,
                "priority": l.priority,
                "state": if l.stopped_at.is_some() { "stopped" } else { "running" },
                "why_state": match state.journal.ledger.state(l.purpose) {
                    Some(WhyState::Alive) => "alive",
                    Some(WhyState::Unknown) => "unknown",
                    Some(WhyState::Dead) => "dead",
                    _ => "absent",
                },
            })
        })
        .collect();
    let queue: Vec<serde_json::Value> = state
        .queue
        .iter()
        .map(|w| {
            serde_json::json!({
                "label": w.label, "peak_bytes": w.peak_bytes,
                "priority": w.priority,
                "waiting_s": w.arrived.elapsed().as_secs(),
            })
        })
        .collect();
    let ledger = state.journal.ledger.counts();
    serde_json::json!({
        "leases": leases,
        "queue": queue,
        "mem_available_bytes": meminfo::mem_available(),
        "chain_hash": format!("{:016x}", state.journal.chain_hash()),
        "purposes": {
            "alive": ledger.alive, "dead": ledger.dead,
            "unknown": ledger.unknown, "absent": ledger.absent,
        },
    })
    .to_string()
}

fn iso_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}Z", now.as_secs(), now.subsec_millis())
}

// WhyLedger is used through Inner.journal.ledger; the import is used in
// type signatures above via the purpose fields.
#[allow(dead_code)]
fn _why_type_witness(_: &WhyLedger) {}
