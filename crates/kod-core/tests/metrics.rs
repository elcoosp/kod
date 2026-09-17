//! Resource metrics as a soft test (§11.3, D6.8).
//!
//! # What this is
//!
//! A single assertion — in the *soft* sense, per the doc's wording:
//! warn, do not fail — that a session's resident memory after a
//! hundred turns stays under a documented ceiling. The point is not
//! to catch a leak in one commit; it is to have a number a reviewer
//! can look at and a threshold a regression can trip.
//!
//! # Why "soft"
//!
//! A hard assertion on RSS is a flake generator. The number depends
//! on allocator behaviour, the kernel's page accounting, and the
//! base image; a CI machine under load can spike by tens of
//! megabytes without the code having changed. A soft threshold
//! catches the case that actually matters — a per-turn leak that
//! pushes RSS into the gigabytes — and stays out of the way for
//! everything else.
//!
//! # Platform coverage
//!
//! Linux reads `/proc/self/status` (VmRSS). macOS would require
//! `mach_task_basic_info`, which is a syscall through `libc` that
//! this crate does not otherwise depend on; the test is skipped
//! there with an explicit message rather than silently passing.
//! Windows has `GlobalMemoryStatusEx` via the `windows-sys` crate,
//! also not a dependency. The test is Linux-only by design; a
//! future platform-specific implementation is a small addition if
//! the number turns out to matter on macOS.

/// RSS in bytes for the current process, or `None` on a platform
/// the test does not support.
#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // The line is like `VmRSS:    12345 kB`.
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn rss_bytes() -> Option<u64> {
    None
}

/// Build a bare engine (no provider), run 100 turns of a trivial
/// input, and report the RSS delta.
///
/// The engine is built with `enable_memory: false` so the
/// measurement does not depend on the redb store's own footprint.
/// The test is about the engine's in-process bookkeeping (transcript
/// map, message metadata, checkpoint table) — the parts that could
/// leak across turns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rss_stays_bounded_after_a_hundred_turns() {
    let Some(before) = rss_bytes() else {
        eprintln!(
            "[metrics] RSS reporting not implemented on this platform; \
             skipping the assertion (this is not a test failure)"
        );
        return;
    };

    // Build a router + engine, both memory-off.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mut cfg = kod_core::router::RouterConfig::default();
    cfg.working_dir = tmp.path().to_path_buf();
    cfg.enable_memory = false;
    let engine = kod_core::KodEngine::new(cfg, tmp.path().join("m.redb"))
        .expect("engine new");
    engine.start().await.expect("engine start");

    // Seed 100 transcript turns directly. `seed_turn` is the same
    // path the TUI uses to restore a session; running 100 full
    // prompts would require a mock provider and would measure the
    // provider's work rather than the engine's bookkeeping. The
    // bookkeeping is what this test exists for.
    for i in 0..100 {
        let user = format!("user message {i} with a bit of content");
        let assistant = format!("assistant reply {i} with a bit of content");
        engine.seed_turn(true, &user).await;
        engine.seed_turn(false, &assistant).await;
    }

    let after = rss_bytes().expect("RSS available once is available twice");
    let delta = after.saturating_sub(before);

    engine.shutdown().await.expect("shutdown");

    // Threshold: 400 MB of RSS growth for 200 short turns. Each
    // turn is ~50 chars; the metadata for a `ChatMessage` is on the
    // order of a few hundred bytes, so the expected growth is well
    // under a megabyte. 400 MB is a generous ceiling — a genuine
    // per-turn leak would blow through it by orders of magnitude,
    // and normal allocator variance stays far below.
    const CEILING: u64 = 400 * 1024 * 1024;
    if delta > CEILING {
        // Soft failure: warn loudly, do not panic. The doc's
        // wording is explicit: "warn en CI, pas fail — baseline à
        // établir".
        eprintln!(
            "[metrics] WARNING: RSS grew {} MB after 100 turns \
             (before {} MB, after {} MB). Ceiling is {} MB. \
             This is a soft threshold — investigate before it becomes \
             a hard failure.",
            delta / (1024 * 1024),
            before / (1024 * 1024),
            after / (1024 * 1024),
            CEILING / (1024 * 1024),
        );
    } else {
        eprintln!(
            "[metrics] RSS delta after 100 turns: {} MB (ceiling {} MB)",
            delta / (1024 * 1024),
            CEILING / (1024 * 1024),
        );
    }
}

/// Sanity check: the RSS reader returns a plausible non-zero value
/// on the platforms where it is implemented. Guards against a
/// silent regression that would make every metric test a no-op.
#[test]
fn rss_reader_is_wired() {
    match rss_bytes() {
        Some(bytes) => {
            assert!(
                bytes > 1024 * 1024,
                "RSS reader returned an implausibly small value: {bytes} bytes",
            );
        }
        None => {
            #[cfg(target_os = "linux")]
            panic!("RSS reader must work on Linux");
            #[cfg(not(target_os = "linux"))]
            eprintln!("[metrics] RSS reader not implemented on this platform (expected)");
        }
    }
}
