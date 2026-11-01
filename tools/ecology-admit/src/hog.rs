//! hog.rs — a capped memory hog for the admission tests.
//!
//! thor-admit's `hog` with the same `--bytes`/`--hold` vocabulary, so
//! the bake-off harness drives both daemons with literally the same
//! command line. Allocates with `mmap` (no zero-fill pass needed — the
//! kernel commits pages on first touch, and we touch them all once so
//! the RSS is real), holds, exits. Prints `hog: done` on success — the
//! line the tests assert on.

use std::time::Duration;

pub fn main(args: &[String]) -> i32 {
    let bytes = flag(args, "--bytes")
        .and_then(|v| crate::sizes::parse_size(&v))
        .unwrap_or(1 << 30);
    let hold: f64 = flag(args, "--hold")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);

    // mmap because Vec would need a zeroing pass; mmap gives zero pages
    // and we touch each one exactly once to commit it.
    let length = bytes as usize;
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
            -1,
            0,
        )
    };
    if mapping == libc::MAP_FAILED {
        eprintln!(
            "hog: mmap of {bytes} bytes failed: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }
    // touch every page (MAP_POPULATE does most of it; belt and braces)
    unsafe {
        let pages = length.div_ceil(4096);
        let base = mapping as *mut u8;
        for page in 0..pages {
            *base.add(page * 4096) = 1;
        }
    }
    std::thread::sleep(Duration::from_secs_f64(hold));
    unsafe {
        libc::munmap(mapping, length);
    }
    println!("hog: done ({bytes} bytes, {hold:.1}s)");
    0
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .map(|i| args[i + 1].clone())
}
