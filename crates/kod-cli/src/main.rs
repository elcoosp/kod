use clap::Parser;
use kod_cli::commands::Cli;

fn main() -> kod_error::Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        println!("KOD version: {}", env!("CARGO_PKG_VERSION"));
        println!("Git commit: {}", env!("GIT_HASH", "unknown"));
        println!("Build time: {}", env!("BUILD_TIME", "unknown"));
    }

    cli.run()?;
    Ok(())
}
