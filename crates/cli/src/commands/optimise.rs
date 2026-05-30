//! `optimise --list <fixture>` — run the optimiser and print the four plans.
//!
//! Two modes (issue #11):
//!
//! * **Fixture mode** (no `--db`): build a private in-memory database, apply
//!   `001_phase0_schema.sql` only, seed from the committed fixtures, and run the
//!   full pipeline. No persistent DB file is written.
//! * **Live mode** (`--db buywise.db`): open the given SQLite file (populated by
//!   `ingest-basketwatch` + `seed-matches`) and run against its real data. This
//!   is the Phase-0 gate path.
//!
//! Both modes share the resolution + constraint-merge pre-step and the same
//! printing. The printed monthly saving (best single store − theoretical
//! minimum, annualised to a month) is the Phase-0 gate metric.

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_core::optimiser::{optimise, OptimiserOutput, Recommendation, StorePlan};
use sqlx::sqlite::SqlitePool;

use crate::commands::basket::{build_optimiser_input, resolve_lines};
use crate::commands::{euros, BasketFixture};
use crate::workspace_root;

/// Weeks per month used to annualise the weekly saving to a monthly figure
/// (52 weeks / 12 months ≈ 4.3333). The Phase-0 gate is stated monthly.
const WEEKS_PER_MONTH: f64 = 52.0 / 12.0;

/// Entry point for `buywise-cli optimise`.
pub fn run(
    list_path: &str,
    db_path: Option<&str>,
    threshold2: Option<i64>,
    threshold3: Option<i64>,
) -> Result<()> {
    // A third-store threshold must be at least the two-store threshold:
    // switching to a third store should never be *easier* to justify than a
    // second one, or the two/three-store switch behaves unexpectedly. Validate
    // up front (both are cents) before any work is done.
    if let (Some(t2), Some(t3)) = (threshold2, threshold3) {
        if t3 < t2 {
            bail!(
                "--threshold3 ({t3} cents) must be >= --threshold2 ({t2} cents): \
                 a third store cannot be easier to justify than a second"
            );
        }
    }
    if threshold3.is_some() {
        // Phase 0 produces single- and two-store plans only; a third-store
        // threshold is accepted for forward-compatibility but has no effect.
        eprintln!("note: --threshold3 is accepted but not used in Phase 0 (no three-store plans)");
    }
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(run_async(list_path, db_path, threshold2))
}

async fn run_async(list_path: &str, db_path: Option<&str>, threshold2: Option<i64>) -> Result<()> {
    let fixture = load_basket(list_path)?;

    let pool = match db_path {
        None => {
            println!("mode: fixture (in-memory; seeded from committed fixtures)");
            let root = workspace_root();
            crate::commands::fixture_db::build_fixture_pool(&root, &fixture.household_id).await?
        }
        Some(path) => {
            println!("mode: live (db={path})");
            open_live_db(path).await?
        }
    };

    let lines = resolve_lines(&pool, &fixture.household_id, &fixture.items).await?;
    let input = build_optimiser_input(&pool, &fixture.household_id, &lines, threshold2).await?;
    let output = optimise(&pool, &input)
        .await
        .context("running the optimiser")?;

    print_output(&output);
    Ok(())
}

/// Open a live SQLite file and ensure its schema is present (idempotent). The
/// file is expected to have been populated by `ingest-basketwatch` +
/// `seed-matches`, both of which already migrate it.
async fn open_live_db(path: &str) -> Result<SqlitePool> {
    let url = format!("sqlite://{path}");
    let pool = db::connect(&url)
        .await
        .with_context(|| format!("opening live database {path}"))?;
    db::run_migrations(&pool)
        .await
        .context("running migrations on live database")?;
    Ok(pool)
}

fn load_basket(path: &str) -> Result<BasketFixture> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let fixture: BasketFixture =
        serde_json::from_str(&raw).with_context(|| format!("parsing {path}"))?;
    Ok(fixture)
}

/// Print the four plans, the recommendation, and the monthly saving estimate.
fn print_output(out: &OptimiserOutput) {
    println!();
    print_plan("BEST SINGLE STORE", out.best_single.as_ref());
    print_plan("BEST TWO-STORE SPLIT", out.best_two_store.as_ref());
    print_plan("THEORETICAL MINIMUM", out.theoretical_minimum.as_ref());
    print_recommendation(out.recommended.as_ref());

    if !out.unresolved_items.is_empty() {
        println!("\nUnresolved ({}):", out.unresolved_items.len());
        for item in &out.unresolved_items {
            println!("  - {}", item.display_text);
        }
    }
    if !out.no_valid_offer_items.is_empty() {
        println!("\nNo valid offer ({}):", out.no_valid_offer_items.len());
        for item in &out.no_valid_offer_items {
            println!("  - {} ({:?})", item.display_text, item.reason);
        }
    }

    print_saving(out);
}

/// Print one plan's header, line items, and totals.
fn print_plan(title: &str, plan: Option<&StorePlan>) {
    println!("== {title} ==");
    let Some(plan) = plan else {
        println!("  (no plan: nothing priceable)\n");
        return;
    };
    let stores: Vec<&str> = plan.stores.iter().map(|r| r.as_db_str()).collect();
    println!("  stores: {}", stores.join(" + "));
    for line in &plan.lines {
        let loyalty = if line.loyalty_applied {
            " (loyalty)"
        } else {
            ""
        };
        println!(
            "  {:<28} {:<10} {} x{} = {}{}",
            truncate(&line.display_text, 28),
            line.retailer.as_db_str(),
            euros(line.unit_price_cents),
            // count is recoverable from line_total / unit_price; print the line total.
            line.line_total_cents / line.unit_price_cents.max(1),
            euros(line.line_total_cents),
            loyalty,
        );
    }
    println!(
        "  total: {}   missing: {}",
        euros(plan.total_cents),
        plan.missing_count
    );
    if let Some(voucher) = &plan.dunnes_voucher_offer_text {
        println!("  Dunnes: {voucher}");
    }
    println!();
}

/// Print the recommendation and the two-store saving.
fn print_recommendation(rec: Option<&Recommendation>) {
    println!("== RECOMMENDED ==");
    let Some(rec) = rec else {
        println!("  (no recommendation)\n");
        return;
    };
    println!("  kind: {:?}", rec.kind);
    let stores: Vec<&str> = rec.plan.stores.iter().map(|r| r.as_db_str()).collect();
    println!("  stores: {}", stores.join(" + "));
    println!("  total: {}", euros(rec.plan.total_cents));
    println!(
        "  two-store saving vs best single: {} (clears threshold: {})",
        euros(rec.two_store_saving_cents),
        rec.clears_convenience_threshold
    );
    println!();
}

/// Print the weekly and monthly saving estimate (the Phase-0 gate metric).
///
/// Two savings are reported against the best single store, both weekly and
/// annualised to a month via [`WEEKS_PER_MONTH`]:
///
/// * **achievable** — best single − theoretical minimum: the most a household
///   could save by buying every item at its globally cheapest store. This is the
///   Phase-0 gate metric (design §1/§10: "realistic monthly savings ≥ €50").
/// * **recommended** — best single − recommended plan: what the *practical*
///   recommendation saves once the second-store convenience threshold is applied
///   (so a tiny split that is not worth the extra trip saves €0 here).
///
/// The final `estimated monthly saving` line is the achievable figure, which is
/// the gate the caller checks.
fn print_saving(out: &OptimiserOutput) {
    let single = out.best_single.as_ref().map(|p| p.total_cents);
    let theoretical = out.theoretical_minimum.as_ref().map(|p| p.total_cents);
    let recommended = out.recommended.as_ref().map(|r| r.plan.total_cents);

    let achievable_weekly = match (single, theoretical) {
        (Some(single), Some(theo)) => (single - theo).max(0),
        _ => 0,
    };
    let recommended_weekly = match (single, recommended) {
        (Some(single), Some(rec)) => (single - rec).max(0),
        _ => 0,
    };
    let achievable_monthly = ((achievable_weekly as f64) * WEEKS_PER_MONTH).round() as i64;
    let recommended_monthly = ((recommended_weekly as f64) * WEEKS_PER_MONTH).round() as i64;

    println!("== SAVING ESTIMATE ==");
    println!(
        "  achievable weekly  (best single − theoretical min):  {}  → monthly {}",
        euros(achievable_weekly),
        euros(achievable_monthly)
    );
    println!(
        "  recommended weekly (best single − recommended plan): {}  → monthly {}",
        euros(recommended_weekly),
        euros(recommended_monthly)
    );
    println!("  estimated monthly saving: {}", euros(achievable_monthly));
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}
