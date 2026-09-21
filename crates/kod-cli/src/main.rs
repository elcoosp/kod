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
//! TUI takes over the terminal after this runs, so anything below
//! `warn` will interleave with the alternate screen — a user who
//! wants trace output in a TUI session should redirect stderr to a
//! file (`kod tui 2>tui.log`) rather than read it off the screen.

use clap::Parser;
use kod_cli::commands::Cli;
use tracing_subscriber::EnvFilter;

fn main() -> kod_error::Result<()> {
    // `EnvFilter::try_from_default_env` reads `RUST_LOG`. The
    // fallback is `warn` — errors and warnings still reach the
    // terminal, but the info-level chatter of a normal session does
    // not.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
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
