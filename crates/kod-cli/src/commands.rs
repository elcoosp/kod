//! Command definitions and handlers for the KOD CLI.

use clap::Parser;
use kod_error::Result;

/// KOD - Terminal-native AI coding agent
#[derive(Parser, Debug)]
#[command(name = "kod", version, about)]
pub struct Cli {
    /// Verbose output
    #[arg(short, long, default_value_t = false)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(clap::Subcommand, Debug)]
pub enum Commands {
    /// Start a chat session
    Chat,
    /// List available skills
    Skills,
    /// Show configuration
    Config,
}

impl Cli {
    pub fn run(&self) -> Result<()> {
        match &self.command {
            Commands::Chat => {
                println!("Starting chat session...");
            }
            Commands::Skills => {
                println!("Listing skills...");
            }
            Commands::Config => {
                println!("Showing configuration...");
            }
        }
        Ok(())
    }
}
