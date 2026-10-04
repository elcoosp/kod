//! `kod` binary entry point.
//!
//! Beyond arg parsing and dispatch, this installs the `tracing`
//! subscriber. Without one, every `tracing::warn!` / `tracing::info!`
//! in the workspace is a no-op — a real problem when a config parse
//! fails (H-D9): the warning fires into the void, and the user sees
//! the built-in defaults with no explanation.
//!
//! The subscriber is installed first, before `Cli::run`, so warnings
//! emitted during config load and engine construction reach the
//! terminal.
//!
//! `RUST_LOG` controls the level filter, defaulting to `warn`. The
//! writer is session-safe: while a CLI subcommand runs, output goes
//! to stderr as before (config-parse warnings must reach the user);
//! once the TUI enters the alternate screen, output goes to
//! `~/.kod/session.log` instead — a raw write onto the live frame
//! garbles the TUI and the diff-based redraw never repairs it. Point
//! users who want verbose TUI logs at that file (or `RUST_LOG=debug`).

use clap::Parser;
use kod_cli::commands::Cli;
use tracing_subscriber::EnvFilter;

fn main() -> kod_error::Result<()> {
    // T1-C3: mark this process un-ptraceable before any sandboxed
    // child is spawned. Landlock does not restrict ptrace(2); without
    // this, a prompt-injected sandboxed child running `ptrace $PPID`
    // reads the kod process's memory, which includes any credential
    // the env-strip tried to remove.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl(PR_SET_DUMPABLE) is a standard libc wrapper.
        let ret = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
        if ret != 0 {
            eprintln!(
                "kod: warning: could not set PR_SET_DUMPABLE=0; the process \
                 remains ptraceable by sandboxed children"
            );
        }
    }

    // `EnvFilter::try_from_default_env` reads `RUST_LOG`. The
    // fallback is `warn` — errors and warnings still reach the
    // terminal, but the info-level chatter of a normal session does
    // not.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // Session-safe writer: stderr until the TUI owns the terminal,
    // then `~/.kod/session.log`. A raw stderr write while the
    // alternate screen is active garbles the ratatui frame (the line
    // lands at the live cursor and the diff-based redraw never
    // repairs the cells), so log output must leave the terminal for
    // the lifetime of the TUI session.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(kod_cli::logging::SessionSafeWriter::default())
        .with_ansi(false)
        .with_target(false)
        .try_init();

    let cli = Cli::parse();

    if cli.verbose {
        println!("KOD version: {}", env!("CARGO_PKG_VERSION"));
        println!("Git commit: {}", env!("GIT_HASH", "unknown"));
        println!("Build time: {}", env!("BUILD_TIME", "unknown"));
    }

    cli.run()?;
    Ok(())
}
