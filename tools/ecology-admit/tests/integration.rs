//! Integration: the daemon's real lifecycle, at a scale any box allows.
//!
//! These spawn the actual binary against a real unix socket with 32 MiB
//! hogs — no GPU, no root, no big allocations. They are `#[ignore]`d by
//! default (socket paths and process groups in CI are a choice, not a
//! given); run them with:
//!
//! ```sh
//! ECO_ADMIT_IT=1 cargo test -p ecology-admit --test integration -- --nocapture
//! ```

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    // CARGO_BIN_EXE_ is set for [[bin]] targets in integration tests
    PathBuf::from(env!("CARGO_BIN_EXE_ecology-admit"))
}

fn wait_for_socket(path: &PathBuf) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the daemon did not come up on {}", path.display());
}

struct Daemon {
    child: std::process::Child,
    socket: PathBuf,
    state: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new(bin())
            .arg("stop")
            .arg("--socket")
            .arg(&self.socket)
            .output();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

fn start(extra: &[&str]) -> Daemon {
    let tag = format!("eco-admit-it-{}-{}", std::process::id(), extra.len());
    let state = std::env::temp_dir().join(&tag);
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let socket = state.join("sock");
    let child = Command::new(bin())
        .arg("daemon")
        .arg("--socket")
        .arg(&socket)
        .arg("--state-dir")
        .arg(&state)
        .arg("--floor")
        .arg("64M")
        .arg("--emergency-floor")
        .arg("512M")
        .arg("--idle-exit-s")
        .arg("0")
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the daemon starts");
    let daemon = Daemon {
        child,
        socket,
        state,
    };
    wait_for_socket(&daemon.socket);
    daemon
}

fn run_hog(
    socket: &PathBuf,
    label: &str,
    peak: &str,
    hold: &str,
    progress: Option<&str>,
) -> String {
    let mut command = Command::new(bin());
    command
        .arg("run")
        .arg("--socket")
        .arg(socket)
        .arg("--label")
        .arg(label)
        .arg("--peak")
        .arg(peak);
    if let Some(every) = progress {
        command.arg("--progress-every").arg(every);
    }
    let output = command
        .arg("--")
        .arg(bin())
        .arg("hog")
        .arg("--bytes")
        .arg("32M")
        .arg("--hold")
        .arg(hold)
        .output()
        .expect("the run client executes");
    String::from_utf8_lossy(&output.stdout).to_string()
        + String::from_utf8_lossy(&output.stderr).as_ref()
}

#[test]
#[ignore = "spawns daemons and hogs; run with ECO_ADMIT_IT=1"]
fn a_lease_runs_checks_in_and_releases() {
    let daemon = start(&[]);
    let output = run_hog(&daemon.socket, "it.32m", "64M", "0.3", None);
    assert!(
        output.contains("hog: done"),
        "the hog did not finish: {output}"
    );
    assert!(
        output.contains("finished: exit 0"),
        "the run client did not report success: {output}"
    );
    // the lease is learned: peaks.jsonl carries the measured peak
    let peaks = std::fs::read_to_string(daemon.state.join("peaks.jsonl")).unwrap();
    assert!(
        peaks.contains("it.32m"),
        "the peak was not learned: {peaks}"
    );
}

#[test]
#[ignore = "spawns daemons and hogs; run with ECO_ADMIT_IT=1"]
fn the_journal_verifies_and_carries_the_lease_story() {
    let daemon = start(&[]);
    let output = run_hog(&daemon.socket, "it.story", "64M", "0.3", None);
    assert!(output.contains("hog: done"), "{output}");

    let verify = Command::new(bin())
        .arg("journal")
        .arg("--state-dir")
        .arg(&daemon.state)
        .arg("--verify")
        .output()
        .expect("journal --verify runs");
    let text = String::from_utf8_lossy(&verify.stdout);
    assert!(
        text.contains("chain intact"),
        "the chain did not verify: {text}"
    );

    let journal = std::fs::read_to_string(daemon.state.join("journal.jsonl")).unwrap();
    for needle in ["\"lease.granted\"", "\"lease.held\"", "\"lease.released\""] {
        assert!(
            journal.contains(needle),
            "the journal is missing {needle}: {journal}"
        );
    }
}

#[test]
#[ignore = "spawns daemons and hogs; run with ECO_ADMIT_IT=1"]
fn a_silent_lease_is_harvested_and_a_healthy_one_is_not() {
    // the stall law at 2 s: the silent hog is harvested + killed while
    // memory is fine (the emergency floor 512 MiB never trips on 32 MiB
    // hogs — the harvest is progress-based, not memory-based)
    let daemon = start(&["--stall-horizon-s", "2"]);

    let healthy = {
        let socket = daemon.socket.clone();
        std::thread::spawn(move || run_hog(&socket, "it.healthy", "64M", "4", Some("1")))
    };
    std::thread::sleep(Duration::from_millis(300));
    let _silent = run_hog(&daemon.socket, "it.silent", "64M", "30", None);

    let deadline = Instant::now() + Duration::from_secs(30);
    let journal_path = daemon.state.join("journal.jsonl");
    let mut saw_harvest = false;
    while Instant::now() < deadline {
        let journal = std::fs::read_to_string(&journal_path).unwrap_or_default();
        if journal.contains("\"stall.harvested\"")
            && journal.contains("\"it.silent\"")
            && journal.contains("\"watchdog.killed\"")
        {
            saw_harvest = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        saw_harvest,
        "the silent lease was never harvested+killed; journal: {}",
        std::fs::read_to_string(&journal_path).unwrap_or_default()
    );

    // the healthy lease must finish on its own, untouched
    let healthy = healthy.join().expect("the healthy run joined");
    assert!(
        healthy.contains("hog: done"),
        "the healthy lease did not finish: {healthy}"
    );
    let journal = std::fs::read_to_string(&journal_path).unwrap();
    let healthy_harvested = journal
        .lines()
        .any(|line| line.contains("stall.harvested") && line.contains("it.healthy"));
    assert!(
        !healthy_harvested,
        "the healthy lease was harvested: {journal}"
    );
}
