//! Memory-footprint measurement (design D6.8).
//!
//! # Why this exists
//!
//! KOD's local-first promise is a small, self-contained binary. The
//! design carries a documented binary-size ceiling (15 MB musl, enforced
//! by the release pipeline's bloat gate), but *runtime* memory has never
//! had a measurement. A regression that turned the memory store
//! quadratic, or that quietly held every turn's full prompt in RAM
//! forever, would not be caught by any test today.
//!
//! This module provides a `rss_bytes()` helper per OS and one soft
//! assertion: an engine lifecycle (construct + start + shutdown) stays
//! under a generous ceiling. The ceiling is deliberately loose — the
//! design (D6.8) calls for a "warn, not fail" baseline that a fresh
//! session has to fit under; a tight bound would flake on a busy CI
//! host or a debug build.
//!
//! # What it does not do
//!
//! - It does not measure the "100 turns" figure the design's prose
//!   mentions. A hundred turns requires a mock provider and a
//!   transcript loop; that belongs with the characterization tests
//!   (which already drive multi-turn behaviour) rather than here.
//!   The single lifecycle check is the regression guard for the memory
//!   subsystem's startup cost.
//! - It does not fail the build. The design is explicit that this is
//!   a soft assertion: print a warning and move on. A hard assertion
//!   on RSS is flaky by nature (allocator behaviour, page cache,
//!   kernel version), and a flaky test is worse than no test.

use std::sync::atomic::AtomicUsize;

/// Resident set size of the current process, in bytes.
///
/// Linux reads `/proc/self/status`'s `VmRSS:` line. macOS shells out to
/// `ps -o rss=` (RSS in KB on Darwin), which is the same number a user
/// would see from `top`. Windows returns `None`: a `GetProcessMemoryInfo`
/// call needs an FFI binding the workspace does not carry, and the two
/// Unix targets are the ones this project builds release binaries for
/// (see `.github/workflows/release.yml`).
pub fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                // Format: `VmRSS:\t   12345 kB`
                let kb: u64 = rest
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let kb: u64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .ok()?;
        Some(kb.saturating_mul(1024))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// A helper for a caller that wants a per-turn delta: record the RSS at
/// one point, record it at another, subtract. Saturates at zero so a
/// measurement that shrank (the allocator returning pages) does not
/// produce a wrapped value.
pub fn rss_delta(before: u64, after: u64) -> u64 {
    after.saturating_sub(before)
}

/// The design's startup ceiling: the process should sit comfortably
/// under 400 MB after a fresh engine lifecycle on any supported host.
const STARTUP_CEILING_BYTES: u64 = 400 * 1024 * 1024;

#[test]
fn rss_measurement_is_available_on_unix() {
    // On Linux and macOS this must return `Some`; on Windows `None` is
    // the documented behaviour and the assertion is skipped.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let rss = rss_bytes().expect("rss_bytes should succeed on Unix");
        assert!(
            rss > 0,
            "a running process must have non-zero RSS, got {rss}",
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        assert!(rss_bytes().is_none(), "Windows is documented as None");
    }
}

#[test]
fn rss_delta_saturates_on_shrink() {
    assert_eq!(rss_delta(1000, 500), 0);
    assert_eq!(rss_delta(500, 1000), 500);
    assert_eq!(rss_delta(1000, 1000), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_lifecycle_stays_under_startup_ceiling() {
    use kod_core::{KodEngine, RouterConfig};
    use tempfile::TempDir;

    // Touch the atomic counter so the import is not dead in a
    // future refactor that removes the sample below.
    let _sentinel = AtomicUsize::new(0);

    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("metrics.redb");
    let cfg = RouterConfig {
        working_dir: tmp.path().to_path_buf(),
        enable_memory: true,
        ..RouterConfig::default()
    };

    let before = rss_bytes();

    let engine = KodEngine::new(cfg, db_path).expect("engine new");
    engine.start().await.expect("engine start");
    engine.shutdown().await.expect("engine shutdown");
    drop(engine);

    let after = rss_bytes();

    // Only enforce the ceiling on the two host OSes the project builds
    // release binaries for. On Windows the measurement is `None`.
    if let Some(after) = after {
        if after > STARTUP_CEILING_BYTES {
            // Soft assertion per design D6.8: warn, do not fail.
            eprintln!(
                "warning: RSS after a fresh engine lifecycle is {} MiB, \
                 above the design's {} MiB ceiling. Not a test failure \
                 — the ceiling is a baseline, not a hard bound.",
                after / (1024 * 1024),
                STARTUP_CEILING_BYTES / (1024 * 1024),
            );
        }

        if let Some(before) = before {
            let delta = rss_delta(before, after);
            // The delta is informational; the design does not set a
            // per-lifecycle delta budget. Print it for the log.
            eprintln!(
                "info: engine lifecycle RSS: before {} KiB, after {} KiB, \
                 delta {} KiB",
                before / 1024,
                after / 1024,
                delta / 1024,
            );
        }
    }
}
