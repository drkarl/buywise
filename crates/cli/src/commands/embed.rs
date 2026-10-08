//! Batch embedding step for the unmatched-line suggestion shortlist (follow-up
//! of the Tesco import, issue #107).
//!
//! `embed-suggestions --db <db> --household-id <uuid>` fills the two vector
//! stores migration 022 adds: one vector per catalogue string (`name_en` /
//! `name_es` / alias) and one per distinct unmatched imported line name. The API
//! then ranks a line's top-5 canonical candidates against those vectors, and the
//! user confirms one — nothing is ever matched automatically.
//!
//! The embedding itself runs in an **external command**, configured through the
//! environment, so this binary carries no ML runtime and no native dependency:
//!
//!   * `EMBEDDING_ENABLED=true` turns the step on (default off);
//!   * `EMBEDDING_COMMAND` is the program to run (for example
//!     `python3 scripts/embed_names.py`); it reads a JSON request on stdin and
//!     prints a JSON response on stdout (see `buywise_core::embedding`);
//!   * `EMBEDDING_MODEL` tags the stored vectors (default `google/embeddinggemma-2`);
//!   * `EMBEDDING_MODEL_PATH` is forwarded to the child as
//!     `BUYWISE_EMBED_MODEL_PATH`; the weights live outside the repository;
//!   * `EMBEDDING_DIM` (default 768) and `EMBEDDING_TIMEOUT_SECS` bound the
//!     expected shape and the wall-clock budget.
//!
//! The step is idempotent: a string already embedded for the configured model is
//! skipped, so re-running it after an import embeds only the new names. Only
//! aggregate counts are printed — never a line name.

use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_core::embedding::{self, CommandEmbedder, Embedder, EmbeddingConfig};

/// Entry point for `buywise-cli embed-suggestions`.
pub fn run(db_path: &str, household_id: &str) -> Result<()> {
    require_existing_db(db_path)?;
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(run_async(db_path, household_id))
}

async fn run_async(db_path: &str, household_id: &str) -> Result<()> {
    let config = EmbeddingConfig::from_env();
    let embedder = CommandEmbedder::new(config.clone());
    if !embedder.is_enabled() {
        bail!(
            "embedding is not configured: set EMBEDDING_ENABLED=true and EMBEDDING_COMMAND \
             (the external embedder to run); see the command's help"
        );
    }

    let pool = db::connect(&format!("sqlite://{db_path}"))
        .await
        .with_context(|| format!("opening database {db_path}"))?;

    let started = Instant::now();
    let catalogue = embedding::embed_catalogue(&pool, household_id, &embedder)
        .await
        .context("embedding the catalogue")?;
    let names = embedding::embed_unmatched_names(&pool, household_id, &embedder)
        .await
        .context("embedding the unmatched line names")?;
    let elapsed = started.elapsed();

    let coverage = embedding::coverage(&pool, household_id, &embedder.model_id())
        .await
        .context("reading suggestion coverage")?;

    println!("embedding model: {}", embedder.model_id());
    println!(
        "  catalogue strings: {} embedded of {} ({} already present)",
        catalogue.embedded,
        catalogue.total,
        catalogue.total - catalogue.embedded
    );
    println!(
        "  unmatched line names: {} embedded of {} ({} already present)",
        names.embedded,
        names.total,
        names.total - names.embedded
    );
    println!("  elapsed: {:.1}s", elapsed.as_secs_f64());
    println!(
        "  coverage: {} of {} unmatched lines ({} of {} distinct names) now have a shortlist",
        coverage.covered_lines,
        coverage.unmatched_lines,
        coverage.covered_names,
        coverage.distinct_names
    );
    Ok(())
}

/// Refuse to run against a database path that does not exist, so a typo cannot
/// create an empty database and silently embed nothing.
fn require_existing_db(db_path: &str) -> Result<()> {
    if !Path::new(db_path).exists() {
        bail!("database {db_path} does not exist");
    }
    Ok(())
}
