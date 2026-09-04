//! Command definitions and handlers for the KOD CLI.

use clap::Parser;
use kod_error::Result;

/// KOD - Terminal-native AI coding agent
#[derive(Parser, Debug)]
#[command(name = "kod", version, about)]
pub struct Cli {
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
