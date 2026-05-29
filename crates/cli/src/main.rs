//! BuyWise CLI entry point.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "buywise-cli", about = "BuyWise developer CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Regenerate fixtures/canonical_items.json and migrations/002_seed.sql from
    /// fixtures/dictionary.json.
    GenerateFixtures,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::GenerateFixtures => buywise_cli::generate::run(),
    }
}
