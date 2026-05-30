//! `validate-basket <fixture>` — resolve a basket fixture and print a table.
//!
//! Builds the in-memory fixture catalogue (canonical items + aliases), resolves
//! every raw line through the Phase-0 bilingual cascade, and prints a per-line
//! resolution table plus a summary resolution rate. This is the cheapest check
//! that the dictionary, normaliser, and matcher agree end-to-end.

use anyhow::{Context, Result};

use crate::commands::basket::{resolve_lines, ResolvedLine};
use crate::commands::BasketFixture;
use crate::workspace_root;

/// Entry point for `buywise-cli validate-basket <fixture_path>`.
pub fn run(fixture_path: &str) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(run_async(fixture_path))
}

async fn run_async(fixture_path: &str) -> Result<()> {
    let fixture = load_basket(fixture_path)?;
    let root = workspace_root();

    // Only the catalogue (canonical items + aliases) is needed for resolution,
    // but reusing the full fixture builder keeps one code path and lets the same
    // command run against a fixture whose items also need pricing later.
    let pool =
        crate::commands::fixture_db::build_fixture_pool(&root, &fixture.household_id).await?;

    let lines = resolve_lines(&pool, &fixture.household_id, &fixture.items).await?;
    print_table(&lines);
    Ok(())
}

/// Load and parse a basket fixture file.
fn load_basket(path: &str) -> Result<BasketFixture> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let fixture: BasketFixture =
        serde_json::from_str(&raw).with_context(|| format!("parsing {path}"))?;
    Ok(fixture)
}

/// Print the resolution table and a summary resolution rate.
fn print_table(lines: &[ResolvedLine]) {
    let rule = "-".repeat(80);
    println!(
        "{:<32}  {:<10}  RESOLVED ID / CANDIDATES",
        "INPUT", "STATUS"
    );
    println!("{rule}");

    let mut resolved = 0usize;
    for line in lines {
        let detail = match &line.outcome {
            buywise_core::matching::ResolutionOutcome::SingleMatch { canonical_item_id } => {
                resolved += 1;
                canonical_item_id.clone()
            }
            buywise_core::matching::ResolutionOutcome::MultipleCandidates { candidates } => {
                format!("{} candidates", candidates.len())
            }
            buywise_core::matching::ResolutionOutcome::NoMatch => "-".to_string(),
        };
        println!(
            "{:<32}  {:<10}  {}",
            truncate(&line.raw, 32),
            line.status(),
            detail
        );
    }

    let total = lines.len();
    let rate = if total == 0 {
        0.0
    } else {
        (resolved as f64 / total as f64) * 100.0
    };
    println!("{rule}");
    println!("resolved {resolved}/{total} ({rate:.1}%)");
}

/// Truncate a string to `max` display columns, appending an ellipsis marker.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}
