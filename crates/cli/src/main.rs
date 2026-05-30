//! BuyWise CLI entry point.
//!
//! Five Phase-0 subcommands (see issue #11):
//!
//! * `validate-basket <fixture>` — resolve a basket fixture and print a
//!   resolution table.
//! * `optimise --list <fixture>` — run the full optimiser. In *fixture mode*
//!   (no `--db`) it builds an in-memory database from the committed fixtures; in
//!   *live mode* (`--db buywise.db`) it reads a populated SQLite file (the
//!   Phase-0 gate path).
//! * `ingest-basketwatch --db <db>` — run the BasketWatch ingest adapter,
//!   writing to the given SQLite file. `--dry-run` forces the no-network path.
//! * `seed-matches --from <csv> --db <db> --household-id <uuid>` — upsert
//!   `product_matches` rows from a CSV (idempotent; `ON CONFLICT … DO UPDATE`).
//! * `generate-fixtures` — regenerate `fixtures/canonical_items.json` and
//!   `migrations/002_seed.sql` from `fixtures/dictionary.json`.

use anyhow::Result;
use clap::{Parser, Subcommand};

use buywise_cli::commands::{ingest, optimise, seed_matches, validate};

#[derive(Parser)]
#[command(name = "buywise-cli", about = "BuyWise developer CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Resolve a basket fixture and print the resolution table.
    ValidateBasket {
        /// Path to a basket fixture (e.g. fixtures/items.json).
        fixture_path: String,
    },

    /// Run the optimiser over a basket and print the four plans + saving.
    Optimise {
        /// Path to a basket fixture (e.g. fixtures/weekly-shop.json).
        #[arg(long)]
        list: String,
        /// SQLite database path. Omit for fixture mode (in-memory, seeded from
        /// the committed fixtures); supply for live mode (the Phase-0 gate).
        #[arg(long)]
        db: Option<String>,
        /// Second-store convenience threshold in cents (overrides the default).
        #[arg(long)]
        threshold2: Option<i64>,
        /// Third-store convenience threshold in cents (accepted for forward
        /// compatibility; three-store plans are not produced in Phase 0).
        #[arg(long)]
        threshold3: Option<i64>,
    },

    /// Run the BasketWatch ingest adapter against a SQLite file.
    IngestBasketwatch {
        /// SQLite database path to write into.
        #[arg(long)]
        db: String,
        /// Skip all network calls (reads fixtures/offers.json) but still write
        /// to the database. Sets BUYWISE_NO_NETWORK=1 internally.
        #[arg(long)]
        dry_run: bool,
    },

    /// Upsert product_matches rows from a CSV into a SQLite file.
    SeedMatches {
        /// CSV path with columns
        /// (canonical_item_id, retailer_product_id, status, matched_by).
        #[arg(long)]
        from: String,
        /// SQLite database path to write into.
        #[arg(long)]
        db: String,
        /// Household UUID the matches belong to (Phase 0:
        /// 00000000-0000-0000-0000-000000000001).
        #[arg(long, value_parser = parse_household_id)]
        household_id: String,
    },

    /// Regenerate fixtures/canonical_items.json and migrations/002_seed.sql from
    /// fixtures/dictionary.json.
    GenerateFixtures,
}

/// Validate `--household-id` parses as a UUID at parse time, so a malformed id
/// fails before any database work. The canonical hyphenated form is returned so
/// downstream string comparisons (against seeded ids) are stable.
fn parse_household_id(s: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(s)
        .map(|u| u.hyphenated().to_string())
        .map_err(|e| format!("invalid household id {s:?}: {e}"))
}

fn main() -> Result<()> {
    // A best-effort tracing subscriber so adapter warnings/errors surface on the
    // CLI. Uses the default (INFO) level to avoid pulling the optional
    // `env-filter` feature of tracing-subscriber. Ignored if a global subscriber
    // is already installed.
    let _ = tracing_subscriber::fmt().try_init();

    let cli = Cli::parse();
    match cli.command {
        Command::ValidateBasket { fixture_path } => validate::run(&fixture_path),
        Command::Optimise {
            list,
            db,
            threshold2,
            threshold3,
        } => optimise::run(&list, db.as_deref(), threshold2, threshold3),
        Command::IngestBasketwatch { db, dry_run } => ingest::run(&db, dry_run),
        Command::SeedMatches {
            from,
            db,
            household_id,
        } => seed_matches::run(&from, &db, &household_id),
        Command::GenerateFixtures => buywise_cli::generate::run(),
    }
}
