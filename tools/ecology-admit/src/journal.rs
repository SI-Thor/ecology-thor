//! journal.rs — the ledger: every decision a chained, replayable line.
//!
//! This module is the concrete answer to the question the builder-ide
//! seat asked — *is the ecology's ledger something thor-admit should
//! adopt?* What it demonstrates, in one file:
//!
//! 1. **The decision log is tamper-evident.** Every line carries the
//!    running FNV-1a chain hash (the same mixer `ecology-core`'s
//!    necrology uses, exported as `fnv1a64`), folded over the line's
//!    own bytes. `journal --verify` re-walks the file and says whether
//!    the chain is intact. A post-mortem can *prove* the log it read is
//!    the log the daemon wrote — no other artifact on the box does that.
//! 2. **A lease is a purpose, not a row.** Each lease is a WHY node
//!    (`ecology_core::why::WhyLedger`): spawned at acquire, served by
//!    `run_under` on grant and progress, *harvested to UNKNOWN* (never
//!    deleted) when the watchdog stops it, revived on parole if it is
//!    re-requested, dead with a reason when the grace runs out. The
//!    morning question "what happened to the trellis run at 3 a.m. and
//!    why" is answered by the state + the chain, not by correlation.
//!
//! The event names in the readable log deliberately mirror thor-admit's
//! (`lease.granted`, `watchdog.stopped`, ...) so the bake-off harness
//! greps both daemons with the same patterns — and so adopting this
//! journal layer under thor-admit, if it wins the bake-off, is a log
//! format change, not a protocol change.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use ecology_core::journal::fnv1a64;
use ecology_core::why::WhyLedger;

/// The chained decision log.
pub struct DecisionJournal {
    chain: u64,
    file: std::fs::File,
    /// The WHY ledger proper: leases are purposes.
    pub ledger: WhyLedger,
    /// Monotonic decision tick (one per decision line).
    tick: u32,
}

impl DecisionJournal {
    /// Open (or create) `journal.jsonl`; resumes the chain from the
    /// last line so a daemon restart continues the same chain.
    pub fn open(path: &Path, capacity: usize) -> std::io::Result<Self> {
        let (seq, chain) = Self::tail_chain(path);
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(DecisionJournal {
            chain,
            file,
            ledger: WhyLedger::new(capacity),
            tick: seq as u32,
        })
    }

    /// Read the file's last (seq, chain) — (0, FNV offset) when empty.
    fn tail_chain(path: &Path) -> (u64, u64) {
        let Ok(file) = std::fs::File::open(path) else {
            return (0, 0xcbf2_9ce4_8422_2325); // FNV-1a offset basis
        };
        let mut last = (0u64, 0xcbf2_9ce4_8422_2325u64);
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let (Some(seq), Some(chain)) = (v["seq"].as_u64(), v["chain"].as_str()) {
                    if let Ok(parsed) = u64::from_str_radix(chain.trim_start_matches("0x"), 16) {
                        last = (seq, parsed);
                    }
                }
            }
        }
        last
    }

    /// Append one decision. The line written is the JSON of the event
    /// with `seq` and `chain` folded in; the chain covers the exact
    /// bytes of the line without the chain field itself (so a verifier
    /// can recompute it from the file alone).
    /// Append one decision; returns its seq (the line's identity in the
    /// file — what a post-mortem cites).
    pub fn record(&mut self, event: serde_json::Value) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        let seq = self.tick as u64;
        // chain over: previous chain + seq bytes + the event's bytes
        let mut mix = self.chain;
        mix = fnv1a64(mix, &seq.to_le_bytes());
        let event_bytes = serde_json::to_string(&event).unwrap_or_default();
        mix = fnv1a64(mix, event_bytes.as_bytes());
        self.chain = mix;

        let line = serde_json::json!({
            "seq": seq,
            "chain": format!("{mix:016x}"),
            "event": event,
        });
        let _ = writeln!(self.file, "{line}");
        let _ = self.file.flush();
        // the stdout mirror: thor-admit's daemon.log convention, so a
        // harness (or an operator with tail -f) reads both daemons the
        // same way. stderr is unbuffered; stdout when piped is not, so
        // flush explicitly.
        println!("{line}");
        let _ = std::io::stdout().flush();
        seq
    }

    /// The running chain hash — the tamper-evidence summary.
    pub fn chain_hash(&self) -> u64 {
        self.chain
    }
}

/// Re-walk a journal file and verify its chain. Returns
/// `(lines, Ok(()))` or the seq where the chain broke.
pub fn verify(path: &Path) -> Result<u64, u64> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Err(0),
    };
    let mut chain: u64 = 0xcbf2_9ce4_8422_2325;
    let mut count: u64 = 0;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            return Err(count + 1);
        };
        let Some(seq) = v["seq"].as_u64() else {
            return Err(count + 1);
        };
        let Some(claimed) = v["chain"].as_str() else {
            return Err(count + 1);
        };
        let Ok(claimed) = u64::from_str_radix(claimed.trim_start_matches("0x"), 16) else {
            return Err(count + 1);
        };
        let Some(event) = v.get("event") else {
            return Err(count + 1);
        };
        let mut mix = chain;
        mix = fnv1a64(mix, &seq.to_le_bytes());
        mix = fnv1a64(
            mix,
            serde_json::to_string(event).unwrap_or_default().as_bytes(),
        );
        if mix != claimed {
            return Err(seq);
        }
        chain = mix;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        // per-test directory: two tests in one process must not delete
        // each other's journal mid-run
        let name = std::thread::current()
            .name()
            .unwrap_or("test")
            .to_ascii_lowercase()
            .replace([' ', ':'], "-");
        let d =
            std::env::temp_dir().join(format!("eco-admit-journal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_chain_verifies_and_tampering_breaks_it() {
        let d = dir();
        let path = d.join("journal.jsonl");
        {
            let mut journal = DecisionJournal::open(&path, 64).unwrap();
            for i in 0..10 {
                journal.record(serde_json::json!({
                    "event": "lease.granted", "label": format!("hog.{i}"), "peak_bytes": 1 << 30
                }));
            }
        }
        assert_eq!(verify(&path).unwrap(), 10);

        // The chain resumes across a restart, and the file keeps verifying.
        {
            let mut journal = DecisionJournal::open(&path, 64).unwrap();
            journal.record(serde_json::json!({"event": "lease.released", "label": "hog.9"}));
        }
        assert_eq!(verify(&path).unwrap(), 11);

        // Any edit to a historical line breaks the chain at that seq.
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replace("hog.0", "hog.0-tampered");
        std::fs::write(&path, tampered).unwrap();
        assert_eq!(verify(&path), Err(1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_lease_is_a_purpose() {
        // The WHY ledger side: a watchdog stop is a harvest to UNKNOWN,
        // not a delete — the state the morning report reads.
        use ecology_core::ids::EntityId;
        use ecology_core::journal::Reason;
        use ecology_core::why::{SpawnState, WhyState};

        let d = dir();
        let path = d.join("journal.jsonl");
        let mut journal = DecisionJournal::open(&path, 64).unwrap();
        let lease: EntityId = journal
            .ledger
            .spawn(None, 1.0, None, SpawnState::Alive, 1, Reason::Born)
            .unwrap();
        journal.ledger.run_under(lease, 10);
        // the watchdog stops it: harvest to UNKNOWN (questionable, not gone)
        assert!(journal.ledger.doubt(lease, Reason::Custom, 20));
        assert_eq!(journal.ledger.state(lease), Some(WhyState::Unknown));
        // and it can be revived, on parole
        assert!(journal.ledger.revive(lease, Reason::Revived, 30));
        assert_eq!(journal.ledger.state(lease), Some(WhyState::Alive));
        let _ = std::fs::remove_dir_all(&d);
    }
}
