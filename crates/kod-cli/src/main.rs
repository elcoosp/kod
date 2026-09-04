use kod_cli::commands::Cli;
use clap::Parser;

fn main() -> kod_error::Result<()> {
    let cli = Cli::parse();
    cli.run()?;
    Ok(())
}
