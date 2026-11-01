//! ecology-admit — the ecology's memory-admission daemon.
//!
//! One binary, six verbs:
//!
//! * `daemon` — the box's memory admission: leases, queue, watchdog,
//!   stall law, learned peaks, the chained decision journal;
//! * `run` — take a lease, run a command under it, give it back
//!   (`--progress-every S` keeps the lease attesting life);
//! * `status` / `stop` — what is going on / shut it down;
//! * `journal` — read and **verify** the chained decision log (the
//!   forensic artifact: `--verify` re-walks the chain);
//! * `hog` — a capped memory hog for tests (`--bytes 30G --hold 5`).
//!
//! The lease semantics are thor-admit's (builder-ide, `tools/thor-admit`)
//! deliberately — the bake-off (`docs/BAKEOFF.md`) runs the same three
//! tests against both daemons and reads both logs with the same grep
//! patterns. The differences are the ecology's: every decision is a
//! chained journal line, a lease is a WHY purpose (stopped = UNKNOWN,
//! not dead), and a stall is harvested before any memory pressure
//! exists.

mod daemon;
mod history;
mod hog;
mod journal;
mod meminfo;
mod protocol;
mod sizes;

use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = run(&args);
    std::process::exit(code);
}

fn run(args: &[String]) -> i32 {
    let Some(command) = args.get(1).map(String::as_str) else {
        usage();
        return 64;
    };
    let rest = &args[2..];
    match command {
        "daemon" => cmd_daemon(rest),
        "run" => cmd_run(rest),
        "status" => cmd_status(rest),
        "stop" => cmd_stop(rest),
        "journal" => cmd_journal(rest),
        "hog" => hog::main(rest),
        "help" | "--help" | "-h" => {
            usage();
            0
        }
        other => {
            eprintln!("ecology-admit: unknown command {other:?}");
            usage();
            64
        }
    }
}

fn usage() {
    eprintln!(
        "ecology-admit — the ecology's memory-admission daemon\n\
         \n\
         USAGE:\n\
         \x20 ecology-admit daemon [--socket P] [--state-dir D] [--floor 25G]\n\
         \x20    [--emergency-floor 8G] [--sample-interval-ms 250]\n\
         \x20    [--stop-grace-samples 8] [--min-victim-rss 2G]\n\
         \x20    [--stall-horizon-s S] [--idle-exit-s S]\n\
         \x20 ecology-admit run --label L (--peak 30G|--peak auto)\n\
         \x20    [--priority N] [--socket P] [--progress-every S] -- CMD...\n\
         \x20 ecology-admit status [--socket P]\n\
         \x20 ecology-admit stop   [--socket P]\n\
         \x20 ecology-admit journal [--state-dir D] [--verify]\n\
         \x20 ecology-admit hog --bytes 30G [--hold 5]\n\
         \n\
         BOX RULES: heavy work runs under flock ~/lm-wt/heavy.lock with\n\
         >= 25 GiB kept free; the box's usable memory is 122 GiB."
    );
}

fn flag(rest: &[String], name: &str) -> Option<String> {
    rest.iter()
        .position(|a| a == name)
        .map(|i| rest[i + 1].clone())
}

fn default_socket() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let uid = unsafe { libc::getuid() };
    PathBuf::from(runtime).join(format!("ecology-admit-{uid}.sock"))
}

fn default_state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/state/ecology-admit")
}

fn cmd_daemon(rest: &[String]) -> i32 {
    let socket = flag(rest, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(default_socket);
    let state_dir = flag(rest, "--state-dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_state_dir);
    let mut config = daemon::Config::defaults(socket, state_dir);
    if let Some(v) = flag(rest, "--floor").and_then(|v| sizes::parse_size(&v)) {
        config.floor = v;
    }
    if let Some(v) = flag(rest, "--emergency-floor").and_then(|v| sizes::parse_size(&v)) {
        config.emergency_floor = v;
    }
    if let Some(v) = flag(rest, "--sample-interval-ms").and_then(|v| v.parse().ok()) {
        config.sample_interval = Duration::from_millis(v);
    }
    if let Some(v) = flag(rest, "--stop-grace-samples").and_then(|v| v.parse().ok()) {
        config.stop_grace = v;
    }
    if let Some(v) = flag(rest, "--min-victim-rss").and_then(|v| sizes::parse_size(&v)) {
        config.min_victim_rss = v;
    }
    if let Some(v) = flag(rest, "--stall-horizon-s").and_then(|v| v.parse().ok()) {
        config.stall_horizon = Some(Duration::from_secs(v));
    }
    if let Some(v) = flag(rest, "--idle-exit-s").and_then(|v| v.parse().ok()) {
        config.idle_exit = if v == 0 {
            None
        } else {
            Some(Duration::from_secs(v))
        };
    }

    // SIGTERM (the `stop` verb's mechanism) ends the daemon gracefully.
    // The flag is a static atomic the signal handler sets and the run
    // loop polls — the same shape thor-admit's daemon uses.
    static TERM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    extern "C" fn on_term(_signal: libc::c_int) {
        TERM.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    // The static is the storage; the Arc the daemon polls is a thin
    // pointer wrapper around a leaked copy is wrong — instead the daemon
    // takes &'static AtomicBool, which TERM already is.
    let term: &'static std::sync::atomic::AtomicBool = &TERM;

    // the pid file: how `stop` finds us
    let pid_file = config.socket.with_extension("pid");
    if let Some(parent) = pid_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&pid_file, format!("{}", std::process::id()));

    let daemon = match daemon::Daemon::new(config, term) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ecology-admit: cannot start: {e}");
            return 1;
        }
    };
    match daemon.run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ecology-admit: {e}");
            1
        }
    }
}

fn cmd_run(rest: &[String]) -> i32 {
    let Some(dash) = rest.iter().position(|a| a == "--") else {
        eprintln!("ecology-admit run: no command (put -- before the command)");
        return 64;
    };
    let (mine, command) = rest.split_at(dash);
    let command = &command[1..];
    if command.is_empty() {
        eprintln!("ecology-admit run: no command after --");
        return 64;
    }
    let label = flag(mine, "--label").unwrap_or_else(|| "unnamed".into());
    let peak = flag(mine, "--peak").unwrap_or_else(|| "auto".into());
    let priority: i32 = flag(mine, "--priority")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let socket = flag(mine, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(default_socket);
    let progress_every: Option<u64> = flag(mine, "--progress-every").and_then(|v| v.parse().ok());

    let outcome =
        protocol::run_under_lease(&socket, &label, &peak, priority, command, progress_every);
    match outcome {
        Ok(outcome) => {
            eprintln!(
                "ecology-admit: {label:?} finished: {} (peak {})",
                match outcome.signal {
                    Some(signal) => format!("signal {signal}"),
                    None => format!("exit {}", outcome.code),
                },
                sizes::fmt_gib(outcome.peak_rss_bytes),
            );
            outcome.code
        }
        Err(e) => {
            eprintln!("ecology-admit: {e}");
            1
        }
    }
}

fn cmd_status(rest: &[String]) -> i32 {
    let socket = flag(rest, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(default_socket);
    match protocol::status(&socket) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            eprintln!("ecology-admit: {e}");
            1
        }
    }
}

fn cmd_stop(rest: &[String]) -> i32 {
    let socket = flag(rest, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(default_socket);
    match protocol::stop(&socket) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ecology-admit: {e}");
            1
        }
    }
}

fn cmd_journal(rest: &[String]) -> i32 {
    let state_dir = flag(rest, "--state-dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_state_dir);
    let path = state_dir.join("journal.jsonl");
    if rest.iter().any(|a| a == "--verify") {
        match journal::verify(&path) {
            Ok(lines) => {
                println!("journal verified: {lines} decisions, chain intact");
                0
            }
            Err(seq) => {
                println!("journal BROKEN at seq {seq} — the chain does not reproduce");
                1
            }
        }
    } else {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                print!("{text}");
                0
            }
            Err(e) => {
                eprintln!("ecology-admit: {e}");
                1
            }
        }
    }
}
