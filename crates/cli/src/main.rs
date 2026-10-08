//! BuyWise CLI entry point.
//!
//! Six Phase-0 subcommands (see issues #11 and #90):
//!
//! * `validate-basket <fixture>` — resolve a basket fixture and print a
//!   resolution table.
//! * `optimise --list <fixture>` — run the full optimiser. In *fixture mode*
//!   (no `--db`) it builds an in-memory database from the committed fixtures; in
//!   *live mode* (`--db buywise.db`) it reads a populated SQLite file (the
//!   Phase-0 gate path).
//! * `ingest-basketwatch --db <db>` — run the BasketWatch ingest adapter
//!   (Tesco/Dunnes/SuperValu/Aldi), writing to the given SQLite file.
//!   `--dry-run` forces the no-network path.
//! * `ingest-lidl --db <db>` — run the Lidl direct ingest adapter, writing to
//!   the given SQLite file. `--dry-run` forces the no-network path; live mode
//!   reads `LIDL_FEED_URL`.
//! * `seed-matches --from <csv> --db <db> --household-id <uuid>` — upsert
//!   `product_matches` rows from a CSV (idempotent; `ON CONFLICT … DO UPDATE`).
//! * `generate-fixtures` — regenerate `fixtures/canonical_items.json` and
//!   `migrations/002_seed.sql` from `fixtures/dictionary.json`.
//!
//! Plus one Phase-1 operator subcommand (issue #51):
//!
//! * `set-password <email> --db <db>` — read a new password without echoing it,
//!   argon2id-hash it, and update `users.password_hash` for the matching user.
//!
//! Plus the first-household bootstrap (issue #139):
//!
//! * `bootstrap-household --name <name> --email <email> [--display-name <name>]
//!   --db <db>` — create the very first household and its owner (role `owner`) on
//!   a fresh database, prompting for the owner's password.
//!
//! Plus two household data-control subcommands (issue #119):
//!
//! * `export-household --db <db> --household-id <uuid> [--out <path>]` — write
//!   every household-scoped record as one documented JSON document
//!   (`docs/household-export.md`).
//! * `delete-household --db <db> --household-id <uuid> [--confirm] [--dry-run]`
//!   — remove every household-scoped record in one transaction.
//!
//! Plus the purchase-ledger import (issue #148):
//!
//! * `import-purchases --db <db> --household-id <uuid> --file <json>` — import
//!   one document of purchase transactions and their lines (idempotent by
//!   identity; corrections keep the previous revision). Format:
//!   `docs/purchase-ledger.md`.
//!
//! Plus the Tesco Clubcard history import (issue #107):
//!
//! * `import-tesco --db <db> --household-id <uuid> --file <export.json>` — read
//!   a Tesco Clubcard export (its `orders` section is ignored), import its
//!   purchases into the same ledger, match each line with the existing cascade,
//!   and record frequency evidence for the matched lines. Idempotent by a
//!   computed identity. See `docs/purchase-ledger.md`.
//!
//! Plus the unmatched-line embedding step (follow-up of the Tesco import #107):
//!
//! * `embed-suggestions --db <db> --household-id <uuid>` — embed the catalogue
//!   strings and the distinct unmatched imported line names into the vector
//!   stores the clarification shortlist reads. The embedder is an external
//!   command configured by the environment, so the binary carries no ML runtime.

use anyhow::Result;
use clap::{Parser, Subcommand};

use buywise_cli::commands::{
    bootstrap, embed, household, ingest, ingest_lidl, optimise, purchases, receipts, seed_matches,
    set_password, tesco, validate,
};

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

    /// Run the Lidl direct ingest adapter against a SQLite file.
    ///
    /// Live mode reads `LIDL_FEED_URL` for the feed endpoint and errors clearly
    /// if it is unset (there is no verified default Lidl.ie feed).
    IngestLidl {
        /// SQLite database path to write into.
        #[arg(long)]
        db: String,
        /// Skip all network calls (reads fixtures/lidl_offers.json) but still
        /// write to the database. Sets BUYWISE_NO_NETWORK=1 internally.
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

    /// Set (or rotate) a user's password in `users.password_hash`.
    ///
    /// Prompts for the new password without echoing it (twice, with
    /// confirmation, on a TTY; once from stdin when piped, for scripting),
    /// argon2id-hashes it, and updates the row matching <email>. When piped,
    /// only the first line of stdin is used as the password. The password is
    /// never taken as a plaintext CLI argument. Exits non-zero if no user has
    /// that email.
    SetPassword {
        /// The email of an existing user (matched against `users.email`).
        email: String,
        /// SQLite database path to update.
        #[arg(long)]
        db: String,
    },

    /// Create the very first household and its owner on a fresh database.
    ///
    /// One-time bootstrap for a new deployment: creates the household, the
    /// owner's account (with an argon2id hash of the password you are prompted
    /// for) and an `owner` membership in one transaction. Refuses if the
    /// database already has any user. The database file is created and migrated
    /// if it does not exist, so this is the first command to run on a new
    /// server. After it, sign in as the owner and add the rest of the household
    /// from Settings → Household members.
    BootstrapHousehold {
        /// The household's display name.
        #[arg(long)]
        name: String,
        /// The owner's email — they sign in with this.
        #[arg(long)]
        email: String,
        /// The owner's display name (defaults to the email's local part).
        #[arg(long)]
        display_name: Option<String>,
        /// SQLite database path (created if missing).
        #[arg(long)]
        db: String,
    },

    /// Export one household's records as a single JSON document.
    ///
    /// Covers the household's catalogue (items, aliases, constraints),
    /// preferences (thresholds, store settings, category rules, aisle orders,
    /// purchase dismissals), product matches, lists with their entries and
    /// history, purchase events and match feedback, with the shared reference
    /// rows they point at. The layout is documented in
    /// `docs/household-export.md`. Writes to stdout unless `--out` is given.
    ExportHousehold {
        /// SQLite database path to read.
        #[arg(long)]
        db: String,
        /// Household UUID to export.
        #[arg(long, value_parser = parse_household_id)]
        household_id: String,
        /// Write the JSON to this path instead of stdout.
        #[arg(long)]
        out: Option<String>,
    },

    /// Delete every record BuyWise holds for one household.
    ///
    /// Removes the household, its members, catalogue, preferences, lists,
    /// entries, purchase history, matches and feedback in one transaction.
    /// Shared reference data (categories, retailers, offers, usage counters) is
    /// not household-identifiable and is left in place. Members left with no
    /// other household are removed with their sessions. Backups taken before
    /// the deletion are NOT rewritten — see `docs/data-flows.md`.
    ///
    /// Refuses to run without `--confirm`; `--dry-run` reports what would be
    /// removed and rolls back.
    DeleteHousehold {
        /// SQLite database path to update.
        #[arg(long)]
        db: String,
        /// Household UUID to delete.
        #[arg(long, value_parser = parse_household_id)]
        household_id: String,
        /// Required to actually delete (ignored with `--dry-run`).
        #[arg(long)]
        confirm: bool,
        /// Report the rows that would be deleted, then roll back.
        #[arg(long)]
        dry_run: bool,
        /// Receipt-image directory whose files are removed with the household
        /// (defaults to `RECEIPT_DIR`, else beside the database).
        #[arg(long)]
        receipt_dir: Option<String>,
    },

    /// Delete receipt images whose 30-day retention has expired.
    ///
    /// Removes each expired image's file (tolerating one already gone) and then
    /// its record, so running it twice changes nothing. Run daily by the
    /// `buywise-expire-receipts.timer` systemd timer. The image directory is
    /// `--receipt-dir`, else `RECEIPT_DIR`, else a `receipts/` directory beside
    /// the database.
    ExpireReceipts {
        /// SQLite database path to update.
        #[arg(long)]
        db: String,
        /// Receipt-image directory (defaults to `RECEIPT_DIR`, else beside the database).
        #[arg(long)]
        receipt_dir: Option<String>,
    },

    /// Import purchase transactions into a household's ledger.
    ///
    /// Reads one JSON document in the format documented in
    /// `docs/purchase-ledger.md`, versioned by `format_version`. A transaction
    /// is identified by its `source_transaction_id` when given, otherwise by a
    /// stable fingerprint of retailer, date, total and the ordered line texts:
    /// re-importing the same identity is idempotent (one transaction, one set of
    /// lines), and a corrected import keeps the previous revision for audit.
    /// Unmatched lines are kept, never dropped or guessed. All transactions in
    /// the document are imported in one database transaction. The database must
    /// already be migrated (the API is the usual migrator).
    ImportPurchases {
        /// SQLite database path to write into.
        #[arg(long)]
        db: String,
        /// Household UUID the purchases belong to.
        #[arg(long, value_parser = parse_household_id)]
        household_id: String,
        /// JSON import document path.
        #[arg(long)]
        file: String,
    },

    /// Import a Tesco Clubcard purchase-history export into a household's ledger.
    ///
    /// Reads one Tesco Clubcard export (`purchases`; the `orders` section is
    /// ignored and never stored), maps every purchase onto the existing purchase
    /// ledger with its store type, totals, savings and lines preserved, matches
    /// each line name with the existing cascade, and records frequency evidence
    /// for the matched lines so first-use suggestions use the imported history.
    /// Re-importing the same export is idempotent. Unmatched lines are kept, not
    /// guessed. The database must already be migrated (the API is the usual
    /// migrator).
    ImportTesco {
        /// SQLite database path to write into.
        #[arg(long)]
        db: String,
        /// Household UUID the purchases belong to.
        #[arg(long, value_parser = parse_household_id)]
        household_id: String,
        /// Tesco Clubcard export JSON path (read-only; never copied).
        #[arg(long)]
        file: String,
    },

    /// Embed the catalogue and the unmatched imported line names for the
    /// clarification shortlist.
    ///
    /// Fills the two vector stores the unmatched-line suggestion endpoints read:
    /// one vector per catalogue string (name or alias) and one per distinct
    /// unmatched line name. The embedding runs in an external command configured
    /// by the environment (`EMBEDDING_ENABLED`, `EMBEDDING_COMMAND`,
    /// `EMBEDDING_MODEL`, `EMBEDDING_MODEL_PATH`), so this binary carries no ML
    /// runtime; the weights live outside the repository. Idempotent — only
    /// strings without a vector for the configured model are embedded. Only
    /// aggregate counts are printed, never a line name. The database must
    /// already be migrated.
    EmbedSuggestions {
        /// SQLite database path to read from and write the vectors into.
        #[arg(long)]
        db: String,
        /// Household UUID whose unmatched lines to embed.
        #[arg(long, value_parser = parse_household_id)]
        household_id: String,
    },
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
        Command::IngestLidl { db, dry_run } => ingest_lidl::run(&db, dry_run),
        Command::SeedMatches {
            from,
            db,
            household_id,
        } => seed_matches::run(&from, &db, &household_id),
        Command::GenerateFixtures => buywise_cli::generate::run(),
        Command::SetPassword { email, db } => set_password::run(&email, &db),
        Command::BootstrapHousehold {
            name,
            email,
            display_name,
            db,
        } => bootstrap::run(&name, &email, display_name.as_deref(), &db),
        Command::ExportHousehold {
            db,
            household_id,
            out,
        } => household::export_run(&db, &household_id, out.as_deref()),
        Command::DeleteHousehold {
            db,
            household_id,
            confirm,
            dry_run,
            receipt_dir,
        } => household::delete_run(&db, &household_id, confirm, dry_run, receipt_dir.as_deref()),
        Command::ExpireReceipts { db, receipt_dir } => {
            receipts::expire_run(&db, receipt_dir.as_deref())
        }
        Command::ImportPurchases {
            db,
            household_id,
            file,
        } => purchases::import_run(&db, &household_id, &file),
        Command::ImportTesco {
            db,
            household_id,
            file,
        } => tesco::import_run(&db, &household_id, &file),
        Command::EmbedSuggestions { db, household_id } => embed::run(&db, &household_id),
    }
}
