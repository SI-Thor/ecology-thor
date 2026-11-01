//! history.rs — learned peaks, in the shape the cost model already reads.
//!
//! Every finished lease appends one JSON line to `peaks.jsonl` — the
//! **same schema thor-admit writes** (its design §2.4), so
//! `core/night/cost`'s memory column can read either daemon's history
//! without caring which one ran. `--peak auto` asks for the p95 of the
//! label's history, linear interpolation (NumPy's convention — the same
//! percentile `core/night/cost/model.py` implements).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// One learned-peak record (thor-admit's schema, field for field).
#[derive(Clone, Debug)]
pub struct PeakRecord {
    pub at: String,
    pub label: String,
    pub peak_rss_bytes: u64,
    pub duration_s: f64,
    pub mem_total_bytes: u64,
    pub floor_bytes: u64,
    pub basis: &'static str,
    pub source: &'static str,
}

/// The p95 of the label's measured peaks, or None with no history
/// (a label nobody has measured is refused, never guessed — same law as
/// thor-admit's `--peak auto`).
pub fn p95_peak(history: &[PeakRecord], label: &str) -> Option<u64> {
    let mut values: Vec<u64> = history
        .iter()
        .filter(|r| r.label == label)
        .map(|r| r.peak_rss_bytes)
        .collect();
    if values.is_empty() {
        return None;
    }
    Some(percentile(&mut values, 0.95))
}

/// Linear-interpolation percentile — NumPy's `linear` convention, the
/// same one `core/night/cost/model.py` uses (and the replay tool).
pub fn percentile(values: &mut [u64], q: f64) -> u64 {
    values.sort_unstable();
    if values.len() == 1 {
        return values[0];
    }
    let position = (values.len() - 1) as f64 * q;
    let low = position.floor() as usize;
    let high = position.ceil() as usize;
    if low == high {
        values[low]
    } else {
        let frac = position - low as f64;
        (values[low] as f64 + (values[high] - values[low]) as f64 * frac) as u64
    }
}

/// Append one record to `peaks.jsonl` (one JSON object per line).
pub fn append(path: &Path, record: &PeakRecord) -> std::io::Result<()> {
    let line = serde_json::json!({
        "at": record.at,
        "kind": record.label.split('.').next().unwrap_or(&record.label),
        "label": record.label,
        "host": std::env::var("HOSTNAME").unwrap_or_default(),
        "peak_rss_bytes": record.peak_rss_bytes,
        "peak_gpu_bytes": serde_json::Value::Null,
        "gpu_visible": false,
        "duration_s": record.duration_s,
        "mem_total_bytes": record.mem_total_bytes,
        "floor_bytes": record.floor_bytes,
        "basis": record.basis,
        "source": record.source,
        "peak_source": "rusage",
    });
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

/// Read the whole history back (for `--peak auto` and the `journal`
/// command's report).
pub fn load(path: &Path) -> Vec<PeakRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        out.push(PeakRecord {
            at: v["at"].as_str().unwrap_or("").to_string(),
            label: v["label"].as_str().unwrap_or("").to_string(),
            peak_rss_bytes: v["peak_rss_bytes"].as_u64().unwrap_or(0),
            duration_s: v["duration_s"].as_f64().unwrap_or(0.0),
            mem_total_bytes: v["mem_total_bytes"].as_u64().unwrap_or(0),
            floor_bytes: v["floor_bytes"].as_u64().unwrap_or(0),
            basis: "measured",
            source: "ecology-admit",
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(label: &str, peak: u64) -> PeakRecord {
        PeakRecord {
            at: "2026-11-01T00:00:00Z".into(),
            label: label.into(),
            peak_rss_bytes: peak,
            duration_s: 1.0,
            mem_total_bytes: 125,
            floor_bytes: 25,
            basis: "measured",
            source: "ecology-admit",
        }
    }

    #[test]
    fn p95_of_a_known_label() {
        let history = vec![
            rec("hog.8g", 8 << 30),
            rec("hog.8g", 8 << 30),
            rec("hog.8g", 9 << 30),
        ];
        let p95 = p95_peak(&history, "hog.8g").unwrap();
        assert!((8 << 30..=9 << 30).contains(&p95));
    }

    #[test]
    fn an_unknown_label_is_refused_not_guessed() {
        let history = vec![rec("hog.8g", 8 << 30)];
        assert!(p95_peak(&history, "never-seen").is_none());
    }

    #[test]
    fn the_round_trip_survives_the_file() {
        let dir = std::env::temp_dir().join(format!("eco-admit-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peaks.jsonl");
        let _ = std::fs::remove_file(&path);
        append(&path, &rec("round.trip", 12345)).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].label, "round.trip");
        assert_eq!(loaded[0].peak_rss_bytes, 12345);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
